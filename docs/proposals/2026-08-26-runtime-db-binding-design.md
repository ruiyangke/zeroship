# Runtime DB binding: construction-time schema binding

**Status.** PARTIAL - the descriptor is the shipped sole schema authority
(`crates/zeroship-data-orm/src/descriptor.rs`, over
`zeroship_data_orm::schema_cache`) and `registerModel` is gone, but the private
module map, the artifact/init channel and the isolate-owned binding do not
exist; the descriptor still arrives on `globalThis.__zsRuntimeDescriptor`
(`crates/zeroship-runtime/src/core/init.rs:3445`).

---

## What it is

`env.db` is complete before the first instruction of creator code runs. There is
no registration promise, no readiness gate, no descriptor on `globalThis`, and no
native method creator code can call to alter runtime schema state or supply a
security policy.

### 1. One authority: the descriptor

The runtime descriptor is the data plane's sole schema authority, and the data
plane never reads the catalog. It is generated from the creator's migration DSL,
folded at build time, shipped in the deploy artifact, and immutable for the
isolate's life. It carries the physical layout, including the masking sibling
columns: the field's own column holds the mask, `__zs_raw__<field>` holds the
real value.

`collection_schema(binding, collection)` returns the descriptor's field map or a
typed error (`descriptor.rs:63`). **There is no third state.** A collection the
descriptor does not declare is not a collection this isolate can serve.

Transport: `manifest.runtime_descriptor` -> the worker resolves the blob
(`crates/zeroship-worker/src/sync.rs`) -> `RuntimeState.runtime_descriptor` ->
runtime boot validates the whole value and calls
`NativePlugin::bind_runtime_descriptor`
(`crates/zeroship-runtime/src/core/plugin.rs:91`, invoked at
`crates/zeroship-runtime/src/core/init.rs:2210`) before creator modules
evaluate. The plugin publishes every collection's field map in one synchronous
replacement. The `storage` block survives untouched. The wire shape is v2 and
only v2; a non-v2 descriptor is refused outright
(`sdks/bootstrap/src/install-schema.ts:106`, `:183`).

**The end state replaces the last two hops.** The descriptor arrives through a
generic artifact bag, `zeroship-plugin-db` parses and semantically validates it,
and a private pre-user bootstrap module synchronously creates the JS collection
wrappers. Neither the global nor a public bootstrap module survives. (Not built:
Open 3.)

What one authority costs, stated because nothing else covers it:

- **A well-formed descriptor that is wrong about the database is not detected at
  runtime.** If a deploy goes live before its migration applies, the descriptor
  says `ssn` is the masked column while the database still holds the real value
  there. There is no epoch to mismatch, no introspection to contradict it, and no
  validation pass to refuse the boot. The deploy pipeline's ordering guarantee is
  therefore an **invariant, not a description**, and it is enforced as a
  precondition on the statement that makes a deploy live
  (`schema_precondition_response`, `crates/zeroship-control/src/api.rs:167`,
  reached at `:818`).
- **Mid-life drift is not detected by the worker.** A migration applied while a
  worker runs, with no deploy, leaves that worker serving against a database its
  descriptor no longer describes until it restarts. A restore has the same
  effect, so **roll the workers after a restore** is a procedure, not a
  mechanism.

### 2. Binding and identity

`DbBinding { app_id, deploy_token }` (`crates/zeroship-data-orm/src/binding.rs:15`)
is the shipped identity: a worker thread keeps isolates for one app at several
deploys, so `app_id` alone does not identify the metadata a CRUD receiver was
minted to use. Every production binding is minted from the isolate's own
`ZEROSHIP_DEPLOY_ID`; `cold_start` is `#[cfg(feature = "test-helpers")]` so that
nothing in a shipped binary constructs a deploy identity from an app id alone.

The end-state binding is isolate-owned and carries more:

```rust
pub struct DbIsolateBinding {
    // app_id, deploy_hash, runtime_instance_id, app_incarnation,
    // and the authority domain (system_identifier, timeline_id)
    pub identity: DbRuntimeIdentity,
    pub declared: Option<Arc<RuntimeSchemaDescriptor>>,
    pub resources: Rc<DbThreadResources>,
    pub resolved: RefCell<IsolateResolvedMetadata>,
}
```

It is anchored in a typed V8 isolate slot and cloned into every native object,
transaction view, subscription helper and spawned future, so ownership is
structural.

**The incarnation and the authority domain are identity, not decoration.** A
binding keyed on `app_id` cannot distinguish a handle cloned before a deprovision
from a handle belonging to the live app; without the authority domain, a PITR
rewind resurrects the incarnation token along with whatever holds it.
`system_identifier` names the **cluster**, so a same-cluster PITR preserves it;
`timeline_id` is what moves when recovery rewinds and promotes. Both come from
one cheap read (`pg_control_system()` / `pg_control_checkpoint()`).

The incarnation comparison is **terminal**: a mismatch denies permanently, with
no re-resolution.

**The binding has two states.** The authority domain is observed, never supplied,
because a value handed in by a caller cannot attest which cluster answered.

- **unbound** - constructed, never read. It may perform its first authority read
  and adopt the observed domain, but only in this state.
- **bound** - domain captured. Every later read compares; a mismatch denies
  terminally.

Adoption is fenced by something that does not come from the database, or it is
self-certifying: **a binding may only adopt a domain matching the one its
`runtime_instance_id` was minted under.** A promotion between those two points
invalidates the runtime rather than silently re-homing its bindings. Construction
stays I/O-free (SQLite requires this) because adoption happens at first read.

The comparison machinery ships and is tested: `SchemaEpoch`
(`crates/zeroship-data-orm/src/transaction/reducer/identity.rs:97`),
`Verdict::Deny(DenyReason::StaleAppIncarnation)` at `:254`, `Verdict::ReResolve`
at `:260` and `:266`, with the setup-outcome adapter in `reducer/mod.rs`. **In
production it is a tautology**: the single construction site
(`transaction/driver.rs:144-164`) mints incarnation 0, domain `(0, 0)`, epoch 0,
`LifecycleState::Stable` and an empty `MaskCeiling`, and echoes the expectation
back as the observation, so `classify` can only return `Current`. The producer is
missing, and its record has no home (Open 1, Open 2).

### 3. Privilege follows the process

The repository invariant in `AGENTS.md`. Its two halves as they bear here:

- **If the worker can do it, it is not privileged.** It lives in the app's own
  schema, written by ordinary parameterised SQL, with provenance enforced at the
  Rust call boundary.
- **If it must be privileged, it belongs to a separate service** that runs no
  creator code - the migration service, the CDC relay, the control plane. Never
  to a function the worker calls.

Consequences that are settled: there is no HMAC session anchor and no
`SessionMinter`; `__zeroship_admin` does not exist and nothing replaced it
(`crates/zeroship-data-orm/src/auth/bootstrap.rs`); CDC slot and publication
ownership belongs to the CDC relay. SQLite has no `session_ctx` at all and binds
context through the session actor's per-call state instead - two tiers
disagreeing about where identity is enforced is evidence that the Rust-boundary
one is sufficient.

The cost: there is no in-database record of which actor a worker was acting as.
A future requirement for SQL-side provenance needs the separate service the
invariant points at, not a restoration of deleted definer-rights functions.

The live counter-example is DB-3: app JS reached a privileged unmask call and
could pass `actor: { kind: "auto" }` to read its own PII/PHI/PCI. It is fenced by
`sanitize_app_actor` (`crates/zeroship-data-orm/src/crud/unmask.rs:321`), called
at **five** sites - `v8_classes/masked_value.rs:300` and `:423`, `crud/mod.rs:669`,
`unmask.rs:1514` and `:1629`. Count them with
`grep -rn 'sanitize_app_actor(' crates/zeroship-plugin-db/src`, un-truncated.

### 4. Trust roots

- The **manifest** is the trust root for the runtime descriptor, and it is
  **unsigned**: `crates/zeroship-bundle/src/manifest.rs` carries no signature or
  attestation field, and `deploy_hash` is a digest the control plane computes on
  receipt. Content addressing proves only that the bytes match the hash that was
  requested. Anything carried in the manifest is creator-authored.
- The **deploy pipeline's ordering guarantee** is the trust root for the
  descriptor being true about the database.
- **Worker configuration** is the trust root for the mask-policy ceiling. Nothing
  inside an isolate contributes to it.
- **Column encryption has NO trust root.** Whether a value is encrypted on write
  is decided from the creator-authored descriptor and nothing else, and the
  `encrypted` key gates the entire encryption stage rather than one column's
  branch. Delete the key and no ciphertext is produced, no `__zsbin__<col>`
  marker is deposited, and the statement degrades to a bare `$N`. Two red probes
  drove one document through the real pipeline twice, varying only that key, and
  both stored plaintext: PostgreSQL 18.4 accepted it through the text-params
  funnel and the stored bytes decode to the plaintext; SQLite accepted it and
  `typeof()` reads `text` in a `BLOB` column. Open 7.

The worker does **not** recompute the descriptor hash: both `BlobStore` impls
already recompute SHA-256 and return `HashMismatch`. It preserves provenance
instead, via a `VerifiedRuntimeArtifact { bytes: Arc<[u8]> }` whose only
constructor is private to the worker's blob-fetch module and takes the `get_blob`
result, with a gate arm asserting no other constructor exists.
`DbRuntimeIdentity` carries no descriptor hash, because nothing reads one.

### 5. Plugin initialization

```rust
pub struct PluginInitContext<'a> {
    pub app_id: &'a AppId,
    pub deploy_hash: Option<&'a str>,
    pub runtime_instance_id: RuntimeInstanceId,
    pub app_incarnation: AppIncarnationId,
    pub artifacts: &'a RuntimeArtifactBag,
}
```

`app_incarnation` is required, not optional: without it this context cannot
construct the binding identity, and a plugin built from it would carry no fence
at all. It arrives on the version poll beside `deploy_hash` - the same channel,
since both are per-deploy facts the control plane owns and the worker must not
invent. The authority domain is deliberately **absent**, because it is observed
at first read, not supplied.

`build_instance` becomes fallible. That needs host plumbing that does not exist:
`NativePlugin::build_instance` today receives only an app id and is infallible,
the worker passes a descriptor string plus a global, and `ServerOptions` has no
artifact bag. The plumbing is part of the step, not a detail.

### 6. Private pre-user binding

Private plugin bootstrap modules are **not entries in the creator-visible module
namespace**. `ModuleRegistry` gains a private map and a reverse identity index.

- `load_modules` never inserts a private module into `compiled` and never puts
  its source into `sources`; it needs a parallel `private_sources` map, because
  its BFS hard-fails on an unknown specifier.
- `resolve_callback` reads its `referrer`. The rule is **asymmetric**: a private
  referrer searches `private` then `compiled`; a public referrer searches
  `compiled` only. The second clause is required because the private DB boot
  module statically imports the public `zeroship` facade.
- The dynamic-import host callback is a **second resolver** under the same rule,
  reading `_resource_name` before `registry_lookup`.
- **Identity mechanism.** `compile_module` stamps a fresh `v8::PrimitiveArray`
  into `host_defined_options` (last parameter of `ScriptOrigin::new`); the runtime
  keeps a per-runtime table of the handles stamped into private modules and
  compares **by identity**. A downcast is not available in the `Cargo.lock`-resolved
  v8 147.1.0 - no `impl_try_from!` for `PrimitiveArray`, no `is_primitive_array()`,
  identity comparison only. A referrer with no stamped options never matches,
  which is the fail-closed default.
- `resolve_native` must never gain a `zeroship-internal:*` arm, because
  `__zeroshipNodeBuiltin` returns any such module's namespace with no referrer. A
  gate arm asserts `resolve_native`'s arms are all `node:*`.
  `install_global_bridge` (`crates/zeroship-runtime/src/core/native_modules.rs:73`)
  is deleted from the production vector; its only consumer is Vite's `fetchModule`.
- `bootstrap_modules::source_for` loses its `@zeroship/bootstrap/install-schema`
  and `@zeroship/db/internal` arms and **keeps its `zeroship` arm** - the
  creator-facing `env` facade, which exists because the static BFS does not
  compile dynamically-only imported modules.
- `wrap_with_bootstrap` **rejects** a module list containing any reserved
  specifier. The source map is last-wins with creator modules appended last, and
  the existing `debug_assert!` is a release no-op.
- The nonce is per-runtime, not a process-wide `LazyLock`, and the DB boot
  module's nonce is independent of the kind bridge's. **Secrecy of a specifier is
  worth nothing** - the kind-bridge nonce is readable from `new Error().stack`
  after any throw. The private map is the boundary.
- **The bootstrap entry exports nothing but `default`.** A security invariant:
  creator code can import it as `"./index.js"` and hold a live namespace after
  boot.
- `import.meta` is not a path today (no host callback is installed); a gate arm
  keeps it that way.

The generated private module imports `bindDbCollections` and `descriptorJson` and
calls the former. The bridge exports **only immutable data** - no policy
function, no registration operation, no replication operation.
`bindDbCollections` creates the SDK wrappers, wires relations and transaction
access, defines `env.db.<collection>`, and freezes the surface. It performs no
native call and returns no promise.

`globalThis.__zsRuntimeDescriptor`, `__zsDbPlatform` and `__zsSchemaReady` are
deleted. The last is not cosmetic: creator top-level code runs before
`runtime-entry` assigns it, so an accessor installed first makes the assignment a
silent no-op and dispatch never awaits it.

### 7. Crate boundary and the backend seam

The backend split is a **crate** split, not a module split, and it is done:
`zeroship-data-core`, `zeroship-data-postgres`, `zeroship-data-sqlite` and
`zeroship-data-sql` exist. `docs/proposals/2026-08-31-data-crate-shape.md`
owns the boundary rules and the remaining cuts.

Neutral SPI types: `DbBackendFactory`, `DbBackend`, `OpSession`, `DbTransaction`,
`PrepareRequest`, `DbPlan`, `DbValue`, `DbRows`, route tokens, neutral `DbError`.
One name per type.

**Ownership.** A session owns its resources and carries no borrows. A session
that borrows its pool checkout cannot offer a `finish` returning a `'static`
future, so `Pool::acquire` returns an owned `PoolConnection` with automatic
return and a bounded acquisition deadline. `PgOpSession` drives raw
`BEGIN`/`COMMIT`/`ROLLBACK` and must **not** store `Transaction<'_>`, which holds
`&'a mut Client`. Raw transaction control carries the obligation the borrowing
wrapper discharges: PostgreSQL may answer `COMMIT` with a `ROLLBACK` tag, checked
by `exec_terminal_on_tx`.

**Non-query capabilities** are not query shapes and cannot be expressed as a
`DbPlan`: CDC consumer lifecycle (spawn, retained ownership, pause,
schema-pending, shutdown), key provision, audit insertion (insert-only, never
DDL), and operator lifecycle. Each neutral, each with stated ownership and
signature. Any feature lacking one is deleted rather than left reaching for a
concrete backend.

**The SPI carries no policy-store capability and no PITR capability.** Policy
ownership is resolved before the isolate exists, so an SPI capability for it
would reinstate the owner this design removes. PITR targets are control-plane
state; the data plane neither reads nor acts on a recovery target.

The declared schema is the only schema the SQL builders read. Live introspection
survives only test-gated: `pg_introspect::read_live_schema`
(`crates/zeroship-data-orm/src/backend/postgres/pg_introspect.rs:66`) is reachable only
through a `#[cfg(feature = "test-helpers")]` `SchemaIntrospect` impl
(`crates/zeroship-data-orm/src/backend/postgres/implementation.rs`).

Every source gate scoping to a boundary file must **first assert that the file
exists**. `backend/api.rs` and `backend/factory.rs` do not exist; a negative grep
scoped to a path that was never created matches nothing and reports success.

### 8. Operation context and total decode

```rust
pub struct DbOpContext { collection: CollectionHandle, tx_route: TxRoute, input: OwnedDbOpInput }
```

`OwnedDbOpInput` is produced by a **total, fallible** decode: every
`Object::get`/`Array::get_index` returning `None` aborts with `INVALID_ARGUMENT`
(a key is never skipped, an element is never defaulted to null); a pending V8
exception is re-thrown; each key is read exactly once so a `Proxy` cannot
substitute values after SDK validation; per-array, per-object, node and byte
budgets are enforced before allocation; non-finite numbers, functions and symbols
are rejected rather than coerced to null, because a filter value becoming null
changes the operator to `IS NULL`.

This is tenant isolation, not robustness: a throwing getter on a filter key that
is silently dropped turns `updateMany({tenantId: <throwing getter>, status})`
into `WHERE status = ...` across every tenant's rows. Shipped, with `DecodeError`
in `crates/zeroship-plugin-db/src/v8_bridge.rs`.

Pipeline: validate descriptor membership; singleflight lazy backend
initialization; `backend.prepare(request)` returning an `OpSession` (route plus
session setup); resolve the collection from the descriptor; build and execute on
the same route; decode/decrypt/mask/normalize; commit; emit success-only usage.

### 9. Resolved metadata and delivery paths

`ResolvedCollectionMetadata` carries the collection, the physical facts the
descriptor states, and per-field `SecurityDisposition`
(`VerifiedPlain` | `Encrypted` | `Masked` | `EncryptedAndMasked`). Resolution is
collection-wide: one invalid field rejects the collection before any data SQL.

| Descriptor says | Result |
| --- | --- |
| Collection absent from the descriptor | `COLLECTION_NOT_DECLARED` before backend work |
| Collection declared, app database has no such relation | `SCHEMA_NOT_APPLIED` |
| Field declared plain | `VerifiedPlain` |
| Field declared encrypted / masked | The declared disposition, with the physical columns the `storage` block names |
| Field absent from the descriptor | Excluded from projection, decode and writes |

`None` must never mean both "verified plaintext" and "metadata unavailable",
which is why `collection_schema` returns a typed error rather than an `Option`.

**Delivery paths.** These rules bind every path returning row data. A change
event is resolved exactly as a read is: a `Masked` or `EncryptedAndMasked` parent
is replaced by its masked sibling and the sibling key dropped; an `Encrypted`
parent is dropped absent an authorization a read would honour.

Generated SELECT lists contain only declared logical fields plus required
platform fields; never `SELECT *`, never a physical-only column, and companion
columns are never creator-visible keys on any path.

**The mutation-side producer is suppressed in production, so delivery must be
designed against the WAL consumer.** `broker::is_app_suppressed`
(`crates/zeroship-data-orm/src/broker.rs:806`, called at `:858` and from
`exec.rs:466`) gates the mutation-side publish: when the WAL consumer runs for an
app it owns the publish path for events that isolate writes. The real producer is
`wal_consumer::emit_for_tuple`, which holds no operation context, and `publish` /
`deliver_event` are synchronous with no session and no pool, so nothing the
delivery path needs may require a round trip. Under the CDC relay the projection
moves into the publication column list, so excluded bytes never reach the wire;
the wire contract is that document's.

**Backends must distinguish "no such relation" from "relation present, not
enumerated"** wherever a relation is enumerated at all. The PostgreSQL attribute
scan restricts to `relkind = 'r'`
(`crates/zeroship-data-orm/src/backend/postgres/pg_introspect.rs:101`, `:362`) while the
platform elsewhere models `'r','p','v','m','f'` and publishes `'p'`. A
partitioned creator table's parent is `relkind = 'p'`, so it is invisible to that
scan and "no metadata" reads the same as "no protection needed". That scan is now
test-gated in the data plane; the migration engine has its own introspection and
the rule binds it.

### 10. Mask policy, the ceiling, and key custody

The policy is **code-managed and immutable at runtime**. `defineMaskPolicy` and
the `Symbol.for("@zeroship/db/MaskPolicyState")` slot are deleted outright. A
policy any module in the isolate can write is not a declaration, it is an input:
creator top-level code runs before the bootstrap drains that slot, the Rust side
validates only the classification vocabulary, and the result is persisted
durably - surviving the isolate, the deploy, and deletion of the offending code.
It is reachable from any transitively bundled package, and a second defeat exists
via `@zeroship/db/internal`'s `_flushPendingMaskPolicy`, dynamically importable
by any referrer today. Every pinned workflow isolate replaying an old deploy
re-runs this boot, so a deploy-pinned writer re-persists an old policy over the
current one.

The replacement has two halves:

- The **declared half is untrusted and deploy-scoped**: artifact data, with the
  same standing as the runtime descriptor, since the artifact is unsigned.
  Declared in the creator's codebase, folded at build time, delivered through the
  artifact/init channel, immutable for the isolate's life. Its only security
  property is that it cannot outlive its deploy, which is the entire point of
  moving it out of durable storage.
- The **ceiling is worker configuration**, delivered at worker composition and
  fixed for the isolate's life.

Both are frozen into the binding, and effective permission is
`ceiling INTERSECT draft` computed **once at binding construction**.

**The meet is not a map intersection, and getting that wrong inverts revocation
for `auto`** - the actor with the most access. A ceiling that revokes `auto` must
deny `auto` even when the creator draft does not mention `auto` at all. That
failure is invisible to every same-key fixture, which is why the acceptance arm
pairs it with a granted-path control.

Deleted concretely: `defineMaskPolicy()` and the policy slot
(`sdks/db/src/policy.ts:120`), the `_flushPendingMaskPolicy` /
`_peekPendingMaskPolicy` drain (`policy.ts:178`); the
`zeroship.db.setMaskPolicy` native op
(`crates/zeroship-plugin-db/src/v8_classes/db_platform.rs:100`) and its
`dispatch_set_mask_policy_field` dispatch; the SQLite JSON sidecar
(`<db_dir>/mask_policies.json`,
`crates/zeroship-data-orm/src/backend/sqlite/mask_policy_store.rs:35`); and the broad
`DbPlatform` V8 class with its private slot and creator-facing replication
diagnostics. There is consequently **no `maskPolicyReady` promise and no
readiness gate**.

`mask_policies` stops being a cache and becomes a field on the binding: no lazy
load, no invalidation, no staleness, no write-through refresh. The three
properties a cache must defend cease to have referents.

**Precedent, and one difference not to copy by accident.**
`crates/zeroship-migrate-server/src/policy.rs` already implements
operator-ceiling meet creator-draft for migrations, with a monorepo-owned
CONFINED default ceiling compiled in via `include_str!`. Masking should look like
its neighbour rather than invent a second shape. But that compose is
**escalation-reject** - a draft grant looser than the ceiling is rejected, never
clamped - while masking's meet **clamps**. Both are defensible and they are not
interchangeable. That ceiling is also **DDL-knobs-only** (`CREATE TABLE` /
`CREATE SCHEMA` / `RENAME` / destructive-ops / RLS), with no vocabulary for mask
classifications, so sharing the store would put two unrelated policies under one
name. Its staleness response is "re-submit required", which is right for a
migration awaiting approval and wrong here.

Costs:

- **Changing a mask policy requires a build and a deploy.** No runtime edit, no
  dashboard toggle, no hot path to a looser rule during an incident.
- **Revocation latency becomes worker-roll time.** An operator who lowers a
  ceiling has changed nothing until the workers carrying the old value are gone.
- **Deploy-pinned workflow isolates keep both old halves until evicted.** The set
  is bounded by `max_pinned_isolates_per_app`, so exposure is bounded, and
  force-eviction is the single lever for immediacy.

#### Key custody

One key per app, derived `HKDF(platform_master_key, app_id, key_version)`. There
is no key table and no getter. The platform master key is already operator config
with a validated floor (`crates/zeroship-core/src/config/secrets.rs`).

The reason is cryptographic, and the ordering matters.
`canonical_aad(collection, column, row_pk_bytes)`
(`crates/zeroship-data-orm/src/encryption/aad.rs`) binds domain separation more
tightly than per-column keys ever did: it separates *rows*, which keys never did
at any granularity, and it authenticates the column name, which is the property a
per-column key was reaching for. It binds the wire version FIRST, so the version
byte - unauthenticated framing in the envelope - is covered by the AEAD tag. A
per-column key adds nothing on top.

Storing a root key in the tenant's own database also puts the key beside the
ciphertext it decrypts. `SECURITY DEFINER` stops the tenant *querying* it; it
does nothing against a dump, a backup, a physical replica or a PITR restore, all
of which carry both halves in one artifact.

**What is lost: rotating one column without touching its siblings.** Rotation
survives at app granularity, and its lazy-re-encryption-on-write half is not
implementable today: the key version has no per-row carrier. `key_id` lives in
the column's stored sentinel `zero-migrate:enc:<mode>:<keyId>:<wraps>`, so a column names
exactly one key version at a time; the ciphertext envelope's leading byte is the
**wire-format** version, not a key version, and `unpack` rejects anything but
`0x01`. The insertion point is reserved in the code, and a key version placed in
that header **must** also be bound into the AAD, or a downgrade to an older key
version is not tag-detectable. Named, not solved.

#### The AAD binds the physical column

There is no `storage.aadColumn`. The AEAD binds the physical column the
descriptor already records; a separate field would exist only to keep pre-flip
rows decryptable - rows that do not exist.

**The surviving constraint is that moving an encrypted value is a re-encrypt, not
a rename.** The column name is authenticated, not merely used to look the value
up, so a migration implementing a column move as `ALTER TABLE ... RENAME COLUMN`
produces a table whose every encrypted cell fails tag verification. With no
deployed ciphertext that costs nothing today and cannot be made to cost nothing
later.

### 11. Transactions and connections

Transaction views enumerate collections from the isolate's descriptor and clone
its binding. Each collection carries the exact transaction route alongside the
same identity and descriptor as its parent.

States, transitions, health, ownership, cancellation, deadline, savepoint frames,
effect buffer, terminal outcomes and the admission key are
`docs/proposals/2026-08-26-sc1-transaction-protocol.md`. Two constraints it must
satisfy, both from current behaviour: settlement must never interpret an absent
client as proof that terminal SQL ran, and cancellation must not drop the client
between `take` and the manual restore.

**Explicit transactions** are a first-class owned object with registry identity
`(runtime_instance_id, tx_id)` and a **separate, explicitly chosen admission
key**. Those are different things: unique transaction ids never contend, while an
app-keyed claim deliberately serialises two same-app top-level begins. SC-1
either keeps that serialisation under an `(runtime_instance_id, app_id)`
admission key or removes it deliberately and specifies the resulting concurrency.

Randomized-encryption atomicity depends on the plan variants: establish the
conflict winner's stored id atomically before encryption, preserve
`predicate AND id` in the final mutation, and run the multi-row algorithm in one
internal transaction so behaviour does not change with encryption mode.

**Connections always come from a pool.** `acquire_dedicated_client`
(`crates/zeroship-data-orm/src/backend/postgres/implementation.rs`) is a pooled checkout whose
error arm distinguishes "this worker hit its own pool ceiling" from "the server
is unreachable", so the concurrent-transaction ceiling is the pool's size. The
one exception is logical replication: a session opened with
`replication=database` stays in streaming protocol mode for its whole life, so
there is nothing for a pool to multiplex. The rule for it is **bound and account
for it**, and under the CDC relay it becomes O(1) per cluster.

The inversion this buys: before, one app could exhaust `max_connections` and take
the cluster down for every tenant everywhere; after, one app can occupy the pool
and stall its co-residents on one worker. The second is much better; it is not
nothing (Open 8).

### 12. SQLite and the dev tier

The explicit migration path remains the schema authority; runtime boot applies
nothing.

**There is no per-operation cross-process flock lease.** An exclusive flock
survives for **restore's file swap only** - the one thing WAL does not cover,
since a lock release must not leave a connection bound to an obsolete inode. The
`:memory:` process-local guard is retained.

The actor keeps two connections per session (`tx_conn` for the one explicit
transaction, `op_conn` for autocommit work), so an app's autocommit **reads**
proceed while it holds an open explicit transaction, and cancellation interrupts a
running statement rather than only inter-statement gaps. The acceptance arm is
**reads**, not ops: SQLite has one writer per database on any number of
connections, so an "ops" arm cannot pass. Cancellation is wired end to end -
`transaction/driver.rs:975` -> `cancel_and_reclaim` (`:1032`) -> `TxCanceller` ->
`SqliteCancelHandle::cancel`
(`crates/zeroship-data-orm/src/backend/sqlite/session.rs:1093`) -> `Interrupts::interrupt`.

SQLite serializes top-level transaction admission per
`(thread-resource, app_id, incarnation)`; the cross-isolate non-contention arm is
**PostgreSQL-only**. Extra transaction connections are rejected: the tier has one
isolate per app by construction and SQLite has one writer per database anyway.

App attachment lives entirely in session preparation, reached by every path that
can touch an app file. `ensure_attached` validates app identity and canonical
path and opens the existing file with no-create semantics; a missing file returns
`SCHEMA_NOT_APPLIED` and no empty file is created.

Every path inserted into a SQLite `file:` URI is **percent-encoded**; SQL-quote
escaping is not sufficient, so `?`, `#` or `%` in `db_dir` would otherwise be
parsed by SQLite's URI parser and `?mode=rw` after an existing `?` would not be
the mode parameter.

A non-SQLite dev URL is typed-rejected before anything derives a path
(`assertSqliteDevUrl`, `sdks/vite-plugin/src/dev-database-url.ts:83`, called
inside `resolveDatabaseUrl` at `:142`). A descriptor change in dev restarts the
runtime rather than mutating schema in place
(`restartRuntimeForDescriptorChange`, `sdks/vite-plugin/src/dev-server.ts:1058`,
invoked at `:1220`).

### 13. Caches and bounds

With no per-operation introspection there is no live-metadata cache to bound. The
requirement did not go away with it.

**The encryption `KeyStore` is unbounded and on the hot path.** Its own module
documentation states it: once a `(app_id, key_id)` entry is inserted it stays for
the lifetime of the `KeyStore`, and there is no rotation surface
(`crates/zeroship-data-orm/src/encryption/keys.rs:56`). It holds tenant key
material for every encrypted app the thread has ever served, and it belongs to
the backend, which lives in the thread-local context until backend reset or
thread exit - **not** isolate eviction. Key resolution runs per encrypted column
per returned row on reads and per encrypted field on writes, and the lookup is an
owned `(String, String)` tuple, so it allocates twice on every call including
cache hits.

The fix: resolve the distinct `(app, key)` set once per operation rather than per
cell, make the lookup borrow instead of allocate, and bound the cache with
zeroizing eviction keyed on the full identity.

Three rules every remaining per-app cache commits to:

1. **A bound stated as a number**, the way a retry policy is - and **bound BYTES,
   or bound entries AND cap per-entry column count.** An entry-count bound alone
   does not bound: measured per-entry cost varies about 25x across realistic
   shapes, because it depends on facets rather than column count.
2. **An eviction policy whose key includes the full identity**, so evicting is
   never confused with invalidating.
3. **Per-app state stored hierarchically, behind `Rc`, resolved once per
   operation and threaded through** - not keyed by string concatenation into one
   flat thread-global map, and not `clone()`d per stage.

**Isolate eviction is a pruning hint, not the bound.** A metadata entry is
kilobytes; a V8 isolate is orders of magnitude larger, so metadata entries should
**outnumber** isolate entries under an independent, larger, byte-capped bound.
`max_isolates` defaults to 200 per thread
(`crates/zeroship-worker/src/config.rs:129`), and under LRU churn and CHWBL spill
oscillation evict-then-reload is the common case at target scale, so coupling
metadata lifetime 1:1 to isolate lifetime would re-buy the cost it removes. The
hint is mechanically deliverable for the LRU arm (`evict_lru` runs on the owning
thread); the deprovision arm is not, and no eviction path calls into plugin-db
today.

### 14. Error contract

| Condition | Code | Retryable |
| --- | --- | --- |
| Undeclared collection in a generated deploy | `COLLECTION_NOT_DECLARED` | no |
| Expected relation or app database not present | `SCHEMA_NOT_APPLIED` | no |
| Logical metadata unavailable to raw JS | `DB_SCHEMA_REQUIRED` | no |
| Creator input not totally decodable | `INVALID_ARGUMENT` | no |
| App deprovisioned; its authority record carries a tombstone | `APP_DEPROVISIONED` | no |
| Binding's incarnation does not match the live one | `STALE_APP_INCARNATION` | no |
| Binding's authority domain does not match the cluster/timeline answering | `AUTHORITY_DOMAIN_MISMATCH` | no |

**The table has no retryable code**, and that is a consequence of the descriptor
being the sole authority: every condition worth retrying was a condition of a read
the data plane no longer performs.

`APP_DEPROVISIONED` and `STALE_APP_INCARNATION` are separate on purpose, because
they occur at different times for the same handle. Deprovision leaves the
incarnation in place and sets a tombstone, so a handle carrying A first meets a
tombstone bearing *its own* incarnation. Only once the app is re-provisioned, and
the record holds B, does the same handle fail on mismatch. Collapsing them would
make the audit trail unable to distinguish "the app is gone" from "the app came
back without you".

**Only one of the seven exists in the codebase** - `INVALID_ARGUMENT`.
`collection_not_declared` exists as a *string* raised by `descriptor.rs`. None of
the others may be argued from as though it described shipped behaviour.

### 15. Invariants

1. **Creator code cannot mutate runtime DB metadata**, and cannot supply a
   security policy as an input.
2. **One isolate has one declared descriptor, one declared mask policy, and one
   operator ceiling**, all fixed for its lifetime. All three arrive before the
   isolate exists, none is reloadable, none is cached, so none can be stale. A
   value creator code can influence after boot is an input, not a declaration.
3. **One operation uses one metadata snapshot**, binding every path that returns
   row data, including CDC and live-query delivery. An immutable descriptor makes
   this trivially true; it stays stated because the cheapest way to reintroduce
   the bug is to add a second source of metadata and not notice.
4. **Security metadata fails closed.** A missing, invalid or unparseable
   descriptor prevents data SQL. What this does not cover is a descriptor that is
   well-formed and wrong about the database; the deploy precondition and the
   masking storage flip cover that, not the runtime.
5. **No data-plane path executes DDL.** Schema change belongs to the migration
   service; a runtime that can alter schema is a runtime that can disagree with
   the descriptor describing it. Enforced by *deleting* the DDL-emitting paths,
   not by a classifier that may never traverse them. The vector and spatial index
   helpers, the `__zeroship_migrations` provenance log, `audit.rs` and the lazy
   unmask-audit `CREATE TABLE` are gone; audit tables are provisioned by the
   migration service. Audit insertion survives as an insert-only SPI capability.
   The one lazy `CREATE TABLE IF NOT EXISTS` still spelled in the crate is the
   drift-audit table (`crud/mask_drift.rs:818`, `:852`), and that whole module is
   `#[cfg(any(test, feature = "test-helpers"))]` at `crud/mod.rs:100` with zero
   production callers, so it is not compiled into a release worker. Wiring drift
   detection means provisioning that table in the migration service first.
6. **Raw JavaScript does not mean unverified plaintext.**
7. **No creator-reachable platform capability exists.**
8. **A private module is invisible, not allowlisted.** Secrecy of a specifier is
   never a security boundary.

### 16. Acceptance criteria

Stated as failing-test shapes. **An arm is evidence only if it was built, ran,
and ruled on something** - not built, filtered out, skipped, aborted partway, and
never scheduled are five different ways of printing something other than a red.
The five mechanisms by which this codebase's tests have reported green while
ruling on nothing are in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`.

**Boot and capability isolation.** An invalid descriptor fails before creator
evaluation. Creator top-level code reaches declared collections. Creator code
cannot observe, mutate or retain the descriptor transport. The three globals do
not exist. Creator imports of every `zeroship-internal:*` specifier fail **even
with the exact nonce**, through the static resolver, dynamic import, and
`__zeroshipNodeBuiltin`; that bridge is absent from production isolates. Creator
dynamic import of `@zeroship/db/internal` and
`@zeroship/bootstrap/install-schema` fails, while `await import("zeroship")`
still resolves. The bootstrap namespace exports only `default`. A reserved
specifier in the module list fails boot in **release** builds. No V8 method
registers a model, mutates declared schema, or writes policy, and no policy value
originating in an isolate is persisted.

**Fail-closed access.** A missing, invalid or unparseable descriptor fails boot
and issues no data SQL, asserted with a counting executor - **zero** metadata
round trips, not "none after a failure". Missing metadata is observably distinct
from `VerifiedPlain`. A plaintext-to-masked redeploy cannot reuse a stale
projection. A filter key whose getter throws fails the operation and never
produces a predicate that is a strict subset of the declared filter.

**One arm that should exist and cannot.** A descriptor that is well-formed but
does not match the database is not detected at runtime. Its arm belongs to the
deploy precondition, not here; writing one against today's runtime would be
writing an arm nothing can pass.

**Delivery.** A mask-only field's plaintext never appears in a CDC event, WS
frame, or live-query payload; the test creates the column through a real
migration and reads a real WAL event - a hand-built `ChangeEvent` fixture does
not satisfy it.

**Policy.** A binding constructed under a lowered ceiling denies `unmask` **by an
actor the effective ceiling governs** - **and an unmask that same ceiling still
permits succeeds in the same test**, since a deny-only arm passes on an
implementation where the meet is broken and everything is denied. A ceiling that
revokes `auto` denies `auto` even when the creator draft does not mention `auto`
at all.

**Cost.** The warm-path bound must be re-derived from what `prepare` still does -
session setup - and that measurement has not been made. A flat "at most three
round trips" is non-discriminating: with no epoch read and no per-operation
lease, a warm operation does strictly less than three, so the arm would pass on
an implementation that reintroduced a metadata round trip. The pool has **two**
validation sources, both excluded from the count and asserted separately: a dirty
checkout runs a validation `simple_query` before use, and a *clean* connection
idle beyond 500 ms pays an alive-validation round trip.

**Backend and boundary.** Equivalent resolved metadata across backends for one
logical fixture. No SQLite I/O during construction or binding. A missing app file
returns `SCHEMA_NOT_APPLIED` and creates no file. SQLite URL vectors include `?`,
`#`, `%`. Dropping a caller-side future races the actor command's completion:
cancellation winning interrupts, rolls back and retires before acknowledging;
completion winning yields `AlreadyCompleted` and claims no rollback. Not "cancels
and rolls back" unconditionally, which cannot pass - the actor may commit and
reply before the caller polls.

**Gates.** Every arm declares the number of items it ruled on and a floor that
number must clear, per `tests/lib/gate_arms.sh`.

### 17. Final state

| Former effect | New owner |
| --- | --- |
| Build collection wrappers | Synchronous private pre-user binding |
| Hold declared logical metadata | Isolate-owned immutable binding |
| Verify physical/security metadata | **Nobody, at runtime.** The descriptor asserts it; the deploy precondition makes the assertion true; the storage flip is the second line of defence |
| Select configured backend | The single backend-factory composition point |
| Lower and execute a plan | The selected concrete backend crate |
| Attach SQLite app database | SQLite session preparation |
| Apply schema, and emit any DDL | Migration service / explicit dev migration path |
| Gate request readiness | Nothing; construction completes before creator evaluation |
| Apply mask policy | Worker configuration (operator ceiling) meet deploy artifact (creator draft), resolved once at binding construction |
| Custody of column encryption keys | Derived from the platform master key at the service; no key table, no getter |
| Record a PITR target | Control plane |
| Establish per-request identity | The Rust call boundary in the worker; no SQL-side session |
| Own replication slots and publications | The CDC relay service |
| Fence a stale handle across deprovision | **Unhomed** (Open 1) |

Schema is applied before runtime, bindings are constructed with the runtime, and
every data operation reads the descriptor its deploy was built with.

---

## Why it is this way

These constraints bind any future change to this design.

- **Privilege follows the process, not the function.** A privileged capability
  the worker can call is reachable by whatever reaches the worker. DB-3 is what
  that shape produces, not an accident of one implementation.
- **The descriptor is the only schema authority, and it must stay the only one.**
  A second source reintroduces the "one operation, one snapshot" bug and the
  "absent means carry on" bug in one move.
- **`collection_schema` has no `Option`.** Missing metadata and verified
  plaintext must never share a representation.
- **No DDL in the data plane.** A runtime that can alter schema can disagree with
  the descriptor describing it. Enforcement is by deletion, not classification.
- **The manifest is unsigned.** Nothing downstream may read "verified hash" as
  end-to-end integrity. Everything the manifest carries is creator-authored.
- **The AAD authenticates the physical column name.** Moving an encrypted value
  is a re-encrypt, never a rename. A key version placed in the envelope header
  must also be bound into the AAD, or downgrade is not tag-detectable.
- **Table-name secrecy is not a security boundary.** `db.collection(name)` mints
  a collection for any non-empty string
  (`crates/zeroship-plugin-db/src/v8_classes/db.rs:123`, `:130`).
  *Addressability* must be what authority decides.
- **Module-specifier secrecy is not a security boundary either.** A per-runtime
  nonce leaks through `new Error().stack`. The private map is the boundary.
- **Connections come from a pool; logical replication is the one exception**, and
  the rule for it is bound-and-account, not pool-it.
- **The effective mask permission is a meet computed once**, and it is not a map
  intersection: a ceiling revoking `auto` must deny `auto` when the draft never
  mentions it.
- **`APP_DEPROVISIONED` and `STALE_APP_INCARNATION` stay distinct**, and every
  identity code stays non-retryable. A terminal denial is not improved by trying
  again.
- **The V8 decode is total.** A skipped key or a defaulted element is a
  cross-tenant predicate, not a robustness nit.
- **Every source gate asserts its scoped file exists**, and matches the
  interpolated form of any identifier it hunts.

---

## Open

1. **Where does the app-identity record live?** NEEDS-DECISION. `incarnation`,
   `deprovisioned_at`, the tombstone rule and the authority domain need durable
   storage. Lifecycle was assigned to the migration service, but that service
   shares the application PostgreSQL cluster, while an authority domain must
   survive a restore of that cluster - no document in this set names a home
   satisfying both. Any answer must preserve four properties: a durable tombstone
   that is never deleted; a CAS against an expected incarnation, so a delayed
   cleanup naming A against a record holding B matches nothing and refuses
   (the worker's pending-deprovision set stores bare UUIDs and deprovisions by
   app id alone); an authority domain a restored dump cannot assert about itself;
   and the three error codes staying distinguishable.

2. **Supply the authority producer.** BUILDABLE, 6-10 hours, blocked on Open 1.
   The classifier, all three `Verdict` arms, the setup-outcome adapter and both
   rotation directions ship and are tested; the single production construction
   site (`transaction/driver.rs:144-164`) mints zeros and echoes the expectation
   back as the observation, so the three typed denial codes are unreachable
   outside tests. Change that one function once a record exists, plus the
   `pg_control_system()` / `pg_control_checkpoint()` reader (which occurs today
   only in a driver integration test).

3. **The cutover: private module map, artifact/init channel, isolate binding.**
   BUILDABLE, 40-60 hours. Everything in sections 5 and 6 is unbuilt, plus:
   fallible `build_instance` and the host plumbing it needs; replacement of every
   declared-schema reader; the `db.collection` addressability half; the operator
   ceiling reaching the service at composition. Basis for the estimate: the three
   host files (`modules.rs`, `dynamic_import.rs`, `bootstrap_modules.rs`), the
   worker's `cache.rs` construction path, and 112 `BackendHandle` references in
   `crates/zeroship-plugin-db/src`.

4. **The mask-policy artifact wire.** BUILDABLE, 8-12 hours. The carrier is
   decided (the artifact channel that already carries the descriptor). Owed: the
   manifest field and its schema, the authoring surface, build-time validation,
   the packer emission path, and the runtime read that turns bytes into a
   `MaskPolicy`. The string `mask` appears zero times in
   `crates/zeroship-bundle/src/manifest.rs` and zero times in
   `sdks/vite-plugin/src/zship.ts`, so there is no latent route in either. Blocked
   on Open 5 for the ceiling half.

5. **What is the ceiling's configuration format and source?** NEEDS-DECISION. The
   ceiling is worker configuration, but its format, its configuration source, and
   what the `zeroship serve` and Vite dev vectors read are specified nowhere.
   Those are separate composition points from the worker, and "worker
   configuration" does not name what they read.

6. **Wire the `DbPlan` IR.** BUILDABLE, 12-20 hours.
   `zeroship-data-sql` exists and its shared core, read and search
   families are built, but it is a `[dev-dependencies]` entry of
   `zeroship-plugin-db` and `grep -rn data_query_builder crates/zeroship-plugin-db/src/`
   returns 0. No shipped binary links it. Three items are closed on paper by
   pointing at it and are not actually closed: prepare-once, the role-divergence
   fix, and the plan-family port. The relation and effects families additionally
   follow the CDC relay's wire contract (Open 11).

7. **How is column encryption fenced, on both dialects?** NEEDS-DECISION.
   `encrypted` is read from the creator-authored descriptor at a single
   whole-stage gate, and deleting that key stores plaintext at rest on both
   PostgreSQL and SQLite - confirmed by two red probes through the real write
   pipeline, not inferred. The obvious fix is dialect-split: parameter typing
   works only on PostgreSQL and fights the deliberate schema-blind coercion model
   there; SQLite has no type fence because affinity is a preference. A structural
   fence keyed on a fact the creator cannot author - the physical column type -
   requires reading the live schema, and that reader is test-gated by design.
   Decide together with whether the dormant mask-drift sweep returns; both want
   the same live-schema read and the same cadence answer. Note the signal is
   **not** the raw sibling: `__zs_raw__<col>` marks masking, not encryption, and
   the engine emits it as plain `TEXT` for a masked-but-unencrypted column.

8. **What is the transaction ceiling, and who pays for exhaustion?**
   NEEDS-DECISION. Pooled checkout is shipped, so the ceiling is the pool's size
   rather than unbounded. Three sizing questions remain: what the ceiling should
   be, given the pool is shared across every app on the worker rather than
   per-app; what a transaction does when the pool is exhausted (queue behind the
   SC-1 deadline, or refuse with a typed error - queueing is preferable only
   while the deadline is shorter than the caller's patience); and whether one app
   can starve others. Nothing currently stops one app holding every checkout.
   This is a **new** cross-tenant failure mode created by a change made for
   safety, and it wants an explicit mitigation rather than discovery in
   production.

9. **Must terminal transaction delivery survive process death?** NEEDS-DECISION.
   The round-7 protocol artifact (`docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`)
   requires a supervisor and a durable `FenceJobRegistry`. The contract mentions
   neither and the codebase contains neither. Until this is answered, SC-1's
   implementation is either *write a reducer* or *write a reducer plus a durable
   job system*. That is a multiple, not a detail.

10. **Deterministic encryption mode: build it out or delete it?**
    NEEDS-DECISION. The crypto half is built (`encrypt_deterministic` derives a
    synthetic nonce as `HMAC-SHA256(k_siv, aad || plaintext)`, with tests pinning
    byte-identical output) and the mode selects the AAD shape, dropping
    `row_pk`. But it is **unreachable from the authoring surface**:
    `ColType::Encrypted { of }` carries the inner type and nothing else, and
    lowering hardcodes `"mode": "randomised"`. Since the committed migration set
    is the schema source of truth, this is a code path no creator input can
    reach. The query-by-plaintext design argues a keyed lookup column serves
    randomised mode - the only reachable one - and therefore closes this rather
    than reopening it.

11. **Build the CDC relay service.** BUILDABLE for the service, NEEDS-DECISION
    for its server floor. No CDC crate exists (`ls crates/ | grep -i cdc` is
    empty), and its transport foundation (ntex v3 response streaming plus
    `cyper`) is declared unvalidated by its own author, so no hours estimate is
    honest until that is measured. The decision half: the relay is specified to
    refuse a server major outside `[180000, 190000)`, while
    `deploy/compose/docker-compose.yml:73` pins `postgres:16`. One of the two
    must move. The service is required for a reason beyond slot count: consuming
    a logical slot requires `REPLICATION` on the connecting role, there is no
    narrower grant, and this platform grants it - with `BYPASSRLS` - to the login
    role of the process that executes creator code. That is a present-tense
    violation of the process-privilege invariant, and only moving WAL
    consumption out lets `zeroship_worker` drop both attributes. This design set
    hands the relay three requirements: a wire projection that is a whitelist
    over declared fields (including the broker's `changed_columns`), a fixture
    for the mask-only shape, and the schema-change signal that deleting the
    second authority left subscribers without.

12. **Build the port ledger that makes `zeroship-schema`'s retirement
    measurable.** BUILDABLE, 6-10 hours. Retirement's stated condition is
    "unported == 0", but the ledger that would count unported does not exist
    (`grep -rn source_symbol tests/ crates/` returns zero), so the condition is
    unmeasurable and cannot be asserted by any gate. Current size: 17,275 lines
    over 7 modules, `query.rs` alone 14,309. Current consumers: five manifests -
    `zeroship-data-core`, `zeroship-data-postgres`, `zeroship-data-sqlite`,
    `zeroship-data-sql` and `zeroship-plugin-db` - with 18
    `zeroship_schema` references in `crates/zeroship-plugin-db/src`. One surface
    is security-critical: `validate_collection`'s reserved-`__zeroship` prefix
    check is the sole guardian of that namespace, and it now has a twin in
    `crates/zeroship-data-sql/src/ident.rs`.

13. **Fail closed at boot on a missing or unparseable descriptor.** BUILDABLE,
    4-6 hours. Invariant 4 is enforced per-collection at call time
    (`collection_schema` has no `Option`), not at boot. The JS bootstrap still
    treats an absent `globalThis.__zsRuntimeDescriptor` as "schema-less app; skip
    silently" (`sdks/bootstrap/src/runtime-entry.ts:38`). The boot check is the
    piece that remains; it is subsumed by Open 3 if that lands first.

14. **Restore the end-to-end `collection_not_declared` witness.** BUILDABLE, 3-5
    hours. `distributed_live` was the tree's only live boot of a V8 isolate with
    `env.db` registered and no descriptor. Fixing it to ship a descriptor - which
    is correct - means nothing now observes that error end to end through a real
    isolate against a real PostgreSQL. The path is covered only by unit tests
    (`descriptor.rs`, `crates/zeroship-worker/src/sync.rs`,
    `sdks/bootstrap/tests/dev-entry-descriptor.test.ts`). Getting it back is a
    **second test**, not a loosening of the first.

15. **Where does the PITR placeholder go?** BUILDABLE, 2-4 hours. One live
    statement still names the deleted `__zeroship_admin` schema and therefore
    fails on every database: `crates/zeroship-data-orm/src/backend/postgres/implementation.rs`
    inserts into `__zeroship_admin.pitr_targets`, and its own comment at
    `:846-847` says the schema no longer exists. PITR targets are control-plane
    state; the data plane's `pitr_replay` member and its SQLite stub
    (`crates/zeroship-data-orm/src/backend/sqlite/mod.rs:1688`) go with the table.

---

## History

Deliberation lives in `docs/proposals/2026-08-26-runtime-db-binding-decision-log.md`
(every retraction and operator decision with the measurement that settled it) and
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md` (the set's landing
page). Sibling specifications: SC-1 transaction protocol, SC-2 SQLite actor
protocol, SC-3 `DbPlan` IR and ledger, SC-4 dev and HMR, SC-5 service ownership,
SC-6 ceiling read contract, plus `2026-08-28-cdc-service.md`,
`2026-08-28-deploy-schema-precondition.md` and `2026-08-31-data-crate-shape.md`.
Defects live in the defect register and its closed companion; the ways this
codebase's tests print green while ruling on nothing live in the verification
record.

**DO-NOT notes.** Each records a mistake that would otherwise be remade.

- **Do not reintroduce live introspection as a second schema authority.** It was
  never independent evidence - the `zero-migrate:enc:` / `zero-migrate:mask:` sentinels it parsed are
  emitted by the migration engine out of the same DSL the descriptor is folded
  from, so the catalog could only ever agree with the descriptor or be stale. It
  was also strictly poorer (no `vector` or `geoPoint` tokens, no `vectorDims`, no
  `idPrefix`) and never existed on SQLite at all.
- **Do not give `collection_schema` an `Option` return.** An absent schema
  meaning "carry on" is how L24 happened: `schema = None` yields `SELECT *`,
  returning the plaintext parent column, and the same absence turns the write
  pipeline's encrypt and mask transforms into no-ops.
- **Do not store the mask policy anywhere isolate code can write it.** Creator
  top-level code runs before the bootstrap drains the slot, and the value was
  persisted durably - outliving the isolate, the deploy, and deletion of the
  offending code.
- **Do not key per-app runtime state on `app_id` alone.** The mask-policy map was
  keyed that way, so two pinned isolates of one app at two deploys shared one
  entry and the last boot to run won for both.
- **Do not add a `storage.aadColumn`, and do not implement a column move as
  `ALTER TABLE ... RENAME COLUMN`.** The column name is authenticated by the
  AEAD, so a rename produces a table whose every encrypted cell fails tag
  verification.
- **Do not restore `__zeroship_admin`, a `SECURITY DEFINER` audit writer, or an
  HMAC session anchor.** They were deleted under the process-privilege invariant
  and nothing replaced them, deliberately.
- **Do not use `<field>_masked` or `mask_sibling_column_for_field`.** The storage
  flip shipped: the sibling is `__zs_raw__<field>`
  (`crates/zeroship-schema/src/query.rs:2207`, `raw_column_name` at `:2232`), (DELETED; runtime compilation now lives in `crates/zeroship-data-sql/src/compile.rs`, and migration DDL in `crates/zeroship-migrate-core/src/schema/query.rs`.)
  covered by `crates/zeroship-plugin-db/tests/mask_flip.rs` and documented at
  `docs/reference/db.md:1602`. `mask_sibling_column_for_field` occurs zero times
  in `crates/`.
- **Do not write a source gate scoped to a path that does not exist.** It matches
  nothing and reports success - it passes on today's tree, passes if the refactor
  is abandoned halfway, and passes if someone later deletes the boundary. Assert
  the file exists first.
- **Do not grep for a literal identifier in a gate.** A gate for
  `CREATE SCHEMA "__zeroship_admin"` matched zero lines in `crates/` and `libs/`
  while the schema was nonetheless created, because the real site interpolated
  the identifier. Match the interpolated form and prove the arm against a fixture
  containing the `format!` spelling.
- **Do not read a commit as proof it is in the tree.** A dangling commit reads
  perfectly; `git merge-base --is-ancestor` is what answers the question. Six
  SHAs this document once cited as landed were dangling while their content was
  present under different hashes.
- **Do not trust `sdks/bootstrap/dist/`.** A descriptor version bump fails boot
  with "expected v1" against a correct v2 descriptor when the gitignored dist is
  stale, and `git status` shows nothing.
- **Do not delete a `__zsSchemaReady` consumer without deleting the assignment.**
  Creator top-level code runs before `runtime-entry` assigns it, so an accessor
  installed first makes the assignment a silent no-op and dispatch never awaits
  it.
- **Do not add a `zeroship-internal:*` arm to `resolve_native`.**
  `__zeroshipNodeBuiltin` returns any such module's namespace with no referrer.
- **Do not grep bare names to confirm the deleted index helpers are gone.**
  `ensure_vector_index`, `ensure_spatial_index` and `create_index_with_recovery`
  have zero definitions and zero call sites, but the names still appear about ten
  times in `zeroship-migrate-core` and two test files, every one a comment naming
  the deleted function to say which engine-side renderer replaced it.
- **Do not treat a "what is zero" list as gate-checkable.** A stale citation
  points at something that moved and `tests/doc_citation_gate.sh` catches it; a
  stale zero-list entry points at nothing at all, so no gate can see it and it
  reads as a to-do someone will dutifully re-do. Two entries in this document's
  former zero list had already shipped.
- **Do not enumerate `sanitize_app_actor` call sites through `head`.** A
  truncated enumeration read as a complete one nearly produced a report of a live
  DB-3 regression that does not exist. There are five.
- **Do not let `sanitize_app_actor`'s strip stand in for auditing the rejected
  claim.** The strip is right and stays, but it erases `id` too, so a denied
  audit row carrying a forged `auto` actor is byte-identical to routine anonymous
  traffic. The rejected claim wants its own column. (Kept and flagged: this is
  the open half of a shipped fix, not narration.)
- **Do not read the warm-path round-trip bound as discriminating.** With no epoch
  read and no per-operation lease, a warm operation does strictly less than three
  round trips, so the arm passes on an implementation that reintroduced a
  metadata round trip.

**Rejected alternatives, kept because each has been re-proposed.** Renaming
`registerModel` preserves the wrong lifecycle. Separate crates *per backend* was
never the problem - mixed ownership was, which is why the split landed along
core/vendor/query-builder lines instead. Keeping `BackendHandle` matches but
moving their bodies leaves every caller aware of both dialects. An asynchronous
schema-ready promise has nothing asynchronous left to represent. Mutating schema
metadata during dev HMR loses a testable boundary that a fresh isolate gives for
free. Storing the mask policy in the deploy descriptor *only* pins authorization
to old code, which is why the ceiling is a separate half. A database-resident
schema epoch with a shared lease and per-operation live introspection existed to
make a second authority trustworthy and cheap, and there is no second authority.
A userspace cross-process flock lease for SQLite rebuilds what WAL snapshot
isolation already provides and brings its own starvation and
per-open-file-description problems. Process-wide singleflight needs a
cross-thread wake path nobody has verified.
