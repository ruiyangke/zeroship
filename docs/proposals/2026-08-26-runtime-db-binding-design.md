# Runtime DB binding: replace `registerModel` with construction-time binding

**Date:** 2026-08-26.

**Status:** direction settled. Several of the implementation steps have landed;
the cutover that deletes `registerModel` has not. Four questions are open. See
[What is open](#6-what-is-open-and-what-blocks-each).

**Scope:** `zeroship-plugin-db`, runtime and worker initialization,
`@zeroship/bootstrap`, `@zeroship/db`, the migration/runtime schema lock,
restore, app lifecycle, CDC delivery, and Vite development boot.

This is a breaking pre-launch design. The cutover deletes `registerModel`; there
is no alias, no compatibility mode, and no dual registration path.

---

## How to read this document

This document states the design as it now is. The superseded record - every
retraction, the operator decisions in the order they were taken, and the
measurements that settled each - is
`docs/proposals/2026-08-26-runtime-db-binding-decision-log.md`. That is not an
appendix: several findings exist nowhere else in the tree, and a claim here that
looks under-argued usually has its argument there.

Every figure below says what it counts and against what boundary, so it can be
re-derived rather than trusted. A figure marked **(unverified)** was not
re-derived against the current tree - keep it, but do not argue from it. Line
citations age against the tree; the ones in the decision log are historical
evidence and are deliberately not re-pointed.

---

## 1. What this design is, and what it is not

`registerModel` is an asynchronous runtime handshake for work that no longer
belongs at runtime.

- PostgreSQL migrations are applied before a deploy becomes live.
- SQLite migrations are applied by the explicit development migration path.
- The JavaScript bootstrap already holds the folded runtime descriptor needed to
  construct `env.db.<collection>` wrappers.
- Native CRUD still needs schema metadata, and a mutable thread-local
  registration cache is the wrong place to hold it.

Replace the handshake with construction-time binding:

1. The worker resolves the content-addressed runtime descriptor and preserves
   the bytes `BlobStore` already verified, with their provenance.
2. The runtime transports it through a generic artifact bag; `zeroship-plugin-db`
   parses and semantically validates it before creator evaluation.
3. The plugin creates an isolate-owned `DbIsolateBinding` holding the immutable
   descriptor, the declared mask policy, the resolved effective mask permission,
   and a handle to service-owned shared resources.
4. A private, pre-user bootstrap module synchronously creates the JavaScript
   collection wrappers on `env.db`.
5. Each native operation resolves its collection from the descriptor and issues
   no metadata round trip at all.

There is no registration promise, no registration marker, no descriptor on
`globalThis`, no readiness gate, and no native method creator code can call to
alter runtime schema state or supply a security policy.

### Goals

- Make `env.db` complete before the first instruction of creator code runs.
- Remove all runtime schema-registration calls and readiness chains.
- Scope declared metadata to the isolate and deploy that owns it.
- Let pinned workflow isolates coexist with a current deploy on one thread.
- Make the artifact's descriptor the authority for physical and security
  metadata.
- Give PostgreSQL and SQLite the same metadata-resolution semantics.
- Keep migration application outside the data-plane runtime.
- Remove every creator-reachable privileged capability, including the ability to
  supply a security policy as an argument.
- Fail before SQL when metadata is absent, invalid, or contradictory.
- Do not regress per-operation cost; the warm path gets cheaper, not dearer.

### Non-goals

- No runtime DDL and no replacement registration protocol.
- No change to the migration operation DSL.
- No new crates for PostgreSQL, SQLite, or the backend SPI.
- No post-launch migration or compatibility period.
- **No startup DDL validation.** It is deferred as a future feature, not
  designed here. What it would be for, in one line: catching a database that
  does not match the descriptor the worker was built for, before the worker
  serves a request.

In scope: the transaction settlement machine, randomized-encryption atomicity,
and the V8 decode budget.

---

## 2. The decisions

| # | Decision | What it means | Specified in |
| --- | --- | --- | --- |
| D1 | **No per-column encryption keys** | One key per app, derived `HKDF(platform_master_key, app_id, key_version)`. No key table, no getter | 3.11, and the key-custody note below it |
| D2 | **PITR targets are control-plane state** | The data plane neither reads nor acts on a recovery target; the SPI member that wrote one goes with the table | 3.8 |
| D3 | **The mask policy is code-managed and immutable at runtime** | Declared in the creator codebase, folded at build time, delivered through the artifact/init channel, fixed for the isolate's life | 3.11 |
| D4 | **The operator ceiling is worker configuration** | Immutable per isolate, changed by rolling the workers. Effective permission is `ceiling INTERSECT draft`, computed once at binding construction | 3.11 |
| D5 | **Privilege follows the PROCESS, not the function** | A repository key invariant (`AGENTS.md`, landed in `196622c9b`). No signed session the worker presents on its own behalf; CDC slot and publication ownership belongs to the CDC service | 3.2 |
| D6 | **`__zeroship_admin` is deleted entirely, and there is no schema epoch** | The runtime descriptor is the authority. This is the JPA/Hibernate model: the mapping comes from the code, not from introspection | 3.1 |
| D7 | **The data plane performs NO live introspection** | The descriptor is the sole authority for schema and the data plane never reads the catalog | 3.1 |
| D8 | **There is no `storage.aadColumn`** | The AEAD binds the physical column the descriptor already records. The surviving constraint is that moving an encrypted value is a re-encrypt, not a rename | 3.11 |
| D9 | **Deterministic encryption mode stays as-is** | Not built out, not deleted. Deferred | 6, "Deterministic encryption" |
| D10 | **Connections always come from a pool** | Pooled checkout is the rule. Logical replication connections are the one exception, and the rule for them is bound-and-account | 3.12 |
| D11 | **No DDL in the data plane** | Schema change belongs to `zeroship-migrate`. Every DDL-emitting path leaves `zeroship-plugin-db` | Invariant 5, step 6 |
| D12 | **The descriptor must not get wrong** | The deploy pipeline enforces the ordering it currently only describes, and the masking storage flip lands as a second line of defence | 3.1, 6 |

Each decision's history - what it replaced, what argued for the replaced shape,
and why that argument failed - is one entry in the decision log.

---

## 3. The architecture as it now stands

### 3.1 One authority: the descriptor

**The runtime descriptor is the data plane's sole schema authority, and the data
plane never reads the catalog.** The descriptor is generated from the creator's
migration DSL, folded at build time, shipped in the artifact, and immutable for
the isolate's life. It carries the physical layout, including the sibling
columns of the masking storage flip, which has shipped: the field's own column
holds the mask and `__zs_raw__<field>` holds the plaintext. The runtime respects the
code; it does not inspect the database to discover how to behave.

`crates/zeroship-plugin-db/src/descriptor.rs` is the whole surface:
`collection_schema` returns the descriptor's field map or a typed error, and
**there is no third state** (`descriptor.rs:66-80`). An absent schema used to
mean "carry on", which is how the read path came to fail open; a collection the
descriptor does not declare is not a collection this isolate can serve
(`descriptor.rs:58-65`).

The descriptor reaches the plugin through the deploy artifact:
`manifest.runtime_descriptor` -> the worker resolves the blob
(`crates/zeroship-worker/src/sync.rs:40-70`) -> `RuntimeState.runtime_descriptor`
-> validated and exposed by `setup_globals`
(`crates/zeroship-runtime/src/core/init.rs:3415-3434`) -> `installSchema` walks
it (`sdks/bootstrap/src/install-schema.ts:970`) -> the descriptor store.
The `storage` block survives that chain untouched, because
`normalizeSchema`'s wire-`FieldDef` branch is a shallow spread
(`install-schema.ts:369-372`) and `read_json_arg` filters no keys.

**The last two hops of that chain are what the cutover replaces.** Today the
descriptor arrives on `globalThis` and is fed back in through `registerModel`
per collection; after the cutover it arrives through the artifact bag and the
private bootstrap module, and neither global nor native registration call
exists.

**Why one authority rather than two.** The introspected source was never
independent evidence: the `zsenc:` and `__zsmask:` sentinels it parsed are
emitted by the migration engine out of the same DSL the descriptor is folded
from, so the catalog could only ever agree with the descriptor or be stale
(`descriptor.rs:12-19`). It was also the poorer of the two - its type mapper
could not produce the `vector` or `geoPoint` tokens at all, and it never carried
`vectorDims` or `idPrefix` (`descriptor.rs:20-25`). And it was already absent on
one backend: `runtime_schema_for` had no SQLite introspector, so the entire mask
and encryption feature set has run on the declared schema alone, on that
backend, in shipped code (`descriptor.rs:30-32`).

**What this costs, stated because nothing else in this document covers it.**

- **A well-formed descriptor that is wrong about the database is not detected at
  runtime.** If a deploy goes live before its migration applies, the descriptor
  says `ssn` is the masked column while the database still holds the real value
  there, and the runtime serves plaintext believing it is masked. There is no
  epoch to mismatch, no introspection to contradict it, and no validation pass
  to refuse the boot. **The deploy pipeline's ordering guarantee is therefore an
  invariant, not a description**: "PostgreSQL migrations are applied before a
  deploy becomes live" is what the whole masking story rests on. Under D12 that
  guarantee becomes an enforced precondition on the statement that makes a
  deploy live, designed in
  `docs/proposals/2026-08-28-deploy-schema-precondition.md` and **not yet
  implemented**.
- **Mid-life drift is not detected by the worker.** A migration applied while a
  worker runs, with no deploy, leaves that worker serving against a database its
  descriptor no longer describes until it restarts. This is Hibernate's
  behaviour too. A restore has the same effect, so **roll the workers after a
  restore** is a procedure rather than a mechanism. The signal that lets a
  consumer notice either event is the in-WAL incarnation marker specified in
  `docs/proposals/2026-08-28-cdc-service.md` section 8; nothing in the data
  plane replaces it.

### 3.2 Privilege follows the process

This is a key invariant of the repository, stated in `AGENTS.md` and landed in
`196622c9b`. This document cites it rather than restating it, because a second
phrasing of an invariant is a second thing to keep current. Its two halves as
they bear here:

- **If the worker can do it, it is not privileged.** It lives in the app's own
  schema, written by ordinary parameterised SQL, with provenance enforced at the
  Rust call boundary.
- **If it must be privileged, it belongs to a separate service** that does not
  execute creator code - the migration service, the CDC service, the control
  plane. Never to a function the worker calls.

The tree contains both the proof and the counter-example.
`crates/zeroship-plugin-db/src/audit.rs:20-42` (the file was DELETED by this
design's own implementation, in `ac38fac0e`; read it there) records the full ceremony as a
proposal - a `SECURITY DEFINER` audit writer mediated by an HMAC-signed
PID-keyed session table - and **refuses it**, because app code has no raw SQL
access and the worker pool is the only writer. The counter-example is DB-3: app
JS reached a privileged unmask call and could pass `actor: { kind: "auto" }` to
read its own PII/PHI/PCI at will, patched by `sanitize_app_actor` stripping
reserved system kinds (`sanitize_app_actor`,
`crates/zeroship-plugin-db/src/crud/unmask.rs:305`). That
is not a bug the shape happened to have; it is what the shape produces.

**Consequences taken.** There is no HMAC session anchor and no `SessionMinter`.
CDC slot and publication creation requires `REPLICATION` or superuser, so it
belongs to the CDC service, specified in
`docs/proposals/2026-08-28-cdc-service.md` and not here.

**SQLite has no `session_ctx` at all**, and says so in its own words -
downstream audit paths "bind context through the session actor's per-call state
instead" (`crates/zeroship-data-sqlite/src/lib.rs:1652-1655`,
unverified). Two tiers disagreeing about where identity is enforced is either a
contract-parity break or evidence that one of them is sufficient; here it is the
second.

**The cost.** There is no in-database record of which actor a worker was acting
as. A future requirement for SQL-side provenance - a second client on the same
database, an operator asking "who read this row" without trusting the worker's
own audit row - has no mechanism, and would need the separate service the
invariant points at rather than a restoration of the deleted functions. The
audit trail remains in the app's own schema, written by the worker and trusted
because the worker is trusted; that is narrower than tamper-evident, and
`audit.rs:20-42` is where it is argued.

### 3.3 The binding and its identity

```rust
pub struct DbIsolateBinding {
    // app_id, deploy_hash, runtime_instance_id, app_incarnation,
    // and the authority domain (system_identifier, timeline_id)
    pub identity: DbRuntimeIdentity,
    pub declared: Option<Arc<RuntimeSchemaDescriptor>>,
    pub resources: Rc<DbThreadResources>,  // from Arc<DbService>, SC-5
    pub resolved: RefCell<IsolateResolvedMetadata>,
}
```

The binding is anchored in a typed V8 isolate slot and cloned into every native
object, transaction view, subscription helper and spawned future, so ownership
is structural.

**The incarnation and the authority domain are part of the identity, not
decorations on it.** A binding keyed on `app_id` alone cannot distinguish a
handle cloned before a deprovision from a handle belonging to the live app; and
without the authority domain, a PITR rewind resurrects the incarnation token
along with whatever holds it.

**Fork C - what fences a stale handle.** A durable 128-bit `AppIncarnationId`,
privileged-minted, qualified by the authority domain
`(system_identifier, timeline_id)`, with permanent tombstones. The comparison is
**terminal**: an incarnation mismatch denies permanently, with no
re-resolution.

`system_identifier` identifies the **cluster** - the tree reads it exactly that
way, via `pg_control_system()`
(`libs/compio-postgres/tests/suite/replication_live.rs:477-483`) - so a
same-cluster PITR *preserves* it. `timeline_id` is what moves when recovery
rewinds and promotes, which is why the pair is required and the single value is
not. Both are one cheap read, measured together on PG 16.14 via
`pg_control_system()` and `pg_control_checkpoint()`.

**Nothing in production reads the domain today.** `pg_control_system()` occurs
exactly three times in the tree, all in one driver integration test
(`libs/compio-postgres/tests/suite/replication_live.rs:458-483`, unverified), so
the step that lands this must land the reader, not merely the columns.

**The binding has two states, and the transition is part of the contract.** The
authority domain is observed rather than supplied, because a value handed in by
a caller cannot attest which cluster and timeline actually answered - but a
binding whose *first* read happens after a PITR promotion has nothing older to
compare against and would adopt the new domain as its own. So:

- **unbound** - constructed, never read. It may perform its first authority read
  and adopt the observed domain, but only in this state;
- **bound** - domain captured. Every later read compares, and a mismatch denies
  terminally.

The adoption itself is fenced by something that does not come from the database,
or it is self-certifying: **a binding may only adopt a domain that matches the
one its `runtime_instance_id` was minted under.** A promotion between those two
points invalidates the runtime rather than silently re-homing its bindings.
Construction stays I/O-free, which SQLite requires, because adoption happens at
first read.

**Fork C's state has no home, and that is open.** See section 6.

**Fork A - transaction admission on SQLite.** SQLite serializes top-level
transaction admission per `(thread-resource, app_id, incarnation)`; the
cross-isolate non-contention arm is **PostgreSQL-only**. Extra transaction
connections are rejected: the tier has one isolate per app by construction and
SQLite has one writer per database regardless.

**Fork B - where authority is read.** An authority read never traverses the data
snapshot and never runs under the tenant role. This rule currently has no
client: the ceiling was the only value ever proposed to be read inside an open
creator transaction, and D4 makes it worker configuration. It is stated because
the *rule* is what has to survive - the next authority value someone wants
mid-transaction faces the same `SET LOCAL ROLE` posture
(`crates/zeroship-plugin-db/src/transaction/mod.rs:202-217`, applied at `:540`)
and the same SSI-predicate-lock amplification.

### 3.4 Trust roots

- The **manifest** is the trust root for the runtime descriptor, and it is
  **unsigned**: `crates/zeroship-bundle/src/manifest.rs` carries no signature or
  attestation field, and `deploy_hash` (`:44-48`) is a digest the control plane
  computes on receipt. Content addressing proves only that the bytes match the
  hash that was requested. Nothing downstream may read "verified hash" as
  end-to-end integrity, and anything carried in the manifest is
  creator-authored.
- The **deploy pipeline's ordering guarantee** is the trust root for the
  descriptor being true about the database. See 3.1.
- **Worker configuration** is the trust root for the mask-policy ceiling.
  Nothing inside an isolate contributes to it.
- **Column encryption has NO trust root, and this is measured, not feared.**
  Whether a value is encrypted on write is decided from the creator-authored
  descriptor and nothing else: `crud/mod.rs:2512-2517`
  (`schema_has_encrypted_columns` = `def.get("encrypted").is_some()`) feeds
  `crud/write_pipeline.rs:243`, and that flag gates the **entire** encryption
  stage rather than one column's branch. Delete the key and the stage is skipped,
  so no ciphertext is produced and no `__zsbin__<col>` marker is deposited; the
  builders key on that marker (`crud/bytes_pass.rs:113,151`,
  `zeroship-schema/src/query.rs:201`), so the statement degrades to a bare `$N`.
  Two red probes drove one document through the real pipeline twice, varying only
  the presence of that key, and both confirmed plaintext at rest:

  | | PostgreSQL 18.4 | SQLite |
  | --- | --- | --- |
  | funnel | `query_text_params` (`backend/postgres.rs:312-319`) | `session.exec` (`zeroship-data-sqlite/src/lib.rs:625-632`) |
  | outcome | accepted; stored bytes decode to the plaintext | accepted; `typeof()` = `text` in a `BLOB` column |

  The deploy pipeline's ordering guarantee above does **not** cover this. Ordering
  says the migration ran before the descriptor was served; it says nothing about a
  descriptor whose `encrypted` key was removed after generation, which is exactly
  the case `zeroship-migrate-server`'s `apply.rs` already concedes ("a creator who
  hand-edits both generated files can make them agree about a lie").

  **This forecloses a dialect-uniform fix.** The driver's typed bind path does
  refuse the Postgres case client-side (`WrongType { postgres: Bytea, rust:
  "&str" }`), but the data plane deliberately does not use it: `client.rs:2876-2883`
  documents the text-params funnel as "the server infers each parameter's type
  from its SQL position and a text value implicit-casts to the target column type
  - the coercion model a schema-blind DML assembler needs". On SQLite there is no
  type fence to reach for at all, because affinity is a preference. So parameter
  typing is at best a Postgres-side second layer, and a structural fence keyed on
  a fact the creator cannot author is required for SQLite regardless. Any design
  that claims to close this must say which leg it covers.

  Note the signal to key that fence on is **not** the raw sibling. A
  `__zs_raw__<col>` column marks *masking*, not encryption - the engine's own
  fixture (`crates/zeroship-migrate/tests/sqlite_engine/declarative_sqlite.rs:1150`)
  emits `__zs_raw__ssn` as plain `TEXT` for a masked-but-unencrypted column
  beside `__zs_raw__secret` as `BLOB` for one that is both.

### 3.5 Descriptor contract and transport

`sdks/bootstrap/src/install-schema.ts` defines and validates the v2 wire shape.
`crates/zeroship-bundle/src/manifest.rs` carries its content-addressed blob
reference, and `crates/zeroship-bundle/tests/runtime_descriptor_ingest_test.rs`
proves the emitted JSON round-trips through bundle ingestion. A present but
invalid descriptor is an isolate load error and never degrades to schema-less
mode.

The worker does **not** recompute the descriptor hash: both `BlobStore` impls
already recompute SHA-256 and return `HashMismatch` (`blob.rs:245-251`,
`s3_blob.rs:413-420`) and `sync.rs:49` fetches through `get_blob`. It preserves
provenance instead:

```rust
pub struct VerifiedRuntimeArtifact { bytes: Arc<[u8]> }
```

Its only constructor is private to the worker's blob-fetch module and takes the
`get_blob` result; a gate arm asserts no other constructor exists, so the type
cannot launder unverified bytes. `DbRuntimeIdentity` carries no descriptor hash,
because nothing reads one.

**A descriptor cutover has a build artifact in its blast radius that no
`git status` shows.** When the descriptor version moves, boot fails with
`expected v1` against a correct v2 descriptor if `sdks/bootstrap/dist/` is stale
- the committed `src/` on v2 and the gitignored build output not. `@zeroship/db`'s
dist rebuilt byte-identical, so bootstrap was the only staleness.

### 3.6 Plugin initialization

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
invent.

The authority domain is deliberately **absent** here, for the reason in 3.3.

`build_instance` becomes fallible. This requires host plumbing that does not
exist today: `NativePlugin::build_instance` currently receives only an app id
and is infallible (`plugin.rs:76-82`, `:224-235`), the worker passes a
descriptor string plus a global (`cache.rs:366-408`), and `ServerOptions` has no
artifact bag (`serve.rs:45-71`). That plumbing is part of the step, not a
detail. (All four unverified.)

### 3.7 Private pre-user binding

Private plugin bootstrap modules are **not entries in the creator-visible module
namespace**. `ModuleRegistry` gains a private map and a reverse identity index.

- `load_modules` never inserts a private module into `compiled` and never puts
  its source into `sources`; it needs a parallel `private_sources` map, because
  its BFS hard-fails on an unknown specifier (`modules.rs:183-188`).
- `resolve_callback` reads its `referrer` (today bound as `_referrer` and never
  used, `modules.rs:326`). The rule is **asymmetric**: a private referrer
  searches `private` then `compiled`; a public referrer searches `compiled`
  only. The second clause is required because the private DB boot module
  statically imports the public `zeroship` facade.
- The dynamic-import host callback is a **second resolver** under the same rule,
  reading `_resource_name` (`dynamic_import.rs:214`) before `registry_lookup`,
  which is restricted to `compiled`.
- **Identity mechanism.** `compile_module` stamps a fresh `v8::PrimitiveArray`
  into `host_defined_options` (last parameter of `ScriptOrigin::new`, currently
  `None` at `modules.rs:103`); the runtime keeps a per-runtime table of the
  handles stamped into private modules and compares **by identity**. A downcast
  is not available: in the `Cargo.lock`-resolved v8 147.1.0, `data.rs:458-463`
  provides no `impl_try_from! { Data for PrimitiveArray }` and no
  `is_primitive_array()`, only identity comparison (`:462`). A referrer with no
  stamped options never matches, which is the fail-closed default.
- `resolve_native` must never gain a `zeroship-internal:*` arm, because
  `__zeroshipNodeBuiltin` returns any such module's namespace with no referrer.
  A gate arm asserts `resolve_native`'s arms are all `node:*`.
  `install_global_bridge` is deleted from the production vector; its only
  consumer is Vite's `fetchModule`.
- `bootstrap_modules::source_for` loses its `@zeroship/bootstrap/install-schema`
  and `@zeroship/db/internal` arms and **keeps its `zeroship` arm** - the
  creator-facing `env` facade, which exists because the static BFS does not
  compile dynamically-only imported modules (`bootstrap_modules.rs:66-72`). The
  false claim at `init.rs:354` is corrected in the same patch.
- `wrap_with_bootstrap` **rejects** a module list containing any reserved
  specifier. The source map is last-wins with creator modules appended last
  (`init.rs:2458-2460`, acknowledged at `:2404-2405`), and the existing
  `debug_assert!` (`:2438`) is a release no-op.
- The nonce is per-runtime, not the process-wide `LazyLock` at `init.rs:1607`,
  and the DB boot module's nonce is independent of the kind bridge's.
  **Secrecy of a specifier is worth nothing**: the kind-bridge nonce is readable
  from `new Error().stack` after any throw through `guardedWorkflowFetch`
  (`init.rs:440`), the same fingerprint measured leaking to an anonymous caller
  at `dispatch.rs:214-218`. The private map is the boundary.
- **The bootstrap entry exports nothing but `default`.** A security invariant,
  not a style choice: creator code can import it as `"./index.js"` and hold a
  live namespace after boot.
- `import.meta` is not a path today (no host callback is installed); a gate arm
  keeps it that way.

(Citations in this subsection are unverified.)

The generated private module imports `bindDbCollections` and `descriptorJson`
and calls the former. The bridge exports **only immutable data** - no policy
function, no registration operation, no replication operation.
`bindDbCollections` creates the SDK wrappers, wires relations and transaction
access, defines `env.db.<collection>`, and freezes the surface. It performs no
native call and returns no promise.

Delete `globalThis.__zsRuntimeDescriptor`, `__zsDbPlatform`, and
`__zsSchemaReady`. The last is not cosmetic: creator top-level code runs before
`runtime-entry` assigns it, so an accessor installed first makes the assignment
a silent no-op and dispatch never awaits it (`init.rs:515-518`).

Whether any of this applies to the **dev vector** is SC-4's decision; today the
dev module graph is Vite's and `__zeroshipNodeBuiltin` survives there.

### 3.8 Backend SPI and module boundary

Neutral types: `DbBackendFactory`, `DbBackend`, `OpSession`, `DbTransaction`,
`PrepareRequest`, `DbPlan`, `DbValue`, `DbRows`, route tokens, neutral
`DbError`. One name per type.

**Ownership.** A session owns its resources and carries no borrows. A session
that borrows its pool checkout cannot then offer a `finish` returning a
`'static` future - that does not compile - so the choice is an **owned pooled
lease**, which requires `Pool::get_owned(self: &Rc<Self>) -> OwnedPooledClient`
preserving the borrowed wrapper's return and timeout behaviour. `PgOpSession`
drives raw `BEGIN`/`COMMIT`/`ROLLBACK`; it must **not** store
`Transaction<'_>`, which holds `&'a mut Client` (`transaction.rs:21-30`).

Raw transaction control carries an obligation the borrowing wrapper already
discharges: PostgreSQL may answer `COMMIT` with a `ROLLBACK` tag, which
`transaction.rs:54-59` detects. That check has shipped on the plugin side as
`exec_terminal_on_tx` (`transaction/mod.rs:139`).

**Explicit transactions** are a first-class owned object per SC-1, with registry
identity `(runtime_instance_id, tx_id)` and a **separate, explicitly chosen
admission key**. Those are different things: unique transaction ids never
contend, while today's app-keyed claim deliberately serialises two same-app
top-level begins (`transaction/mod.rs:312-335`, `context.rs:165-193`). SC-1
either keeps that serialisation under an `(runtime_instance_id, app_id)`
admission key or removes it deliberately and specifies the resulting
concurrency.

**Non-query capabilities**: CDC consumer lifecycle (spawn, retained ownership,
pause, schema-pending, shutdown), key provision, audit insertion (insert-only,
never DDL), and operator lifecycle. Each neutral, each with stated ownership and
signatures in SC-3's ledger. Any feature lacking one is deleted rather than left
reaching for a concrete backend. **These are not query shapes and cannot be
expressed as a `DbPlan`** - CDC lifecycle, key management, operator deletion,
backup/restore and persistent transaction ownership all need their own
signatures.

**The SPI carries no policy-store capability and no PITR capability.** Policy
ownership is resolved before the isolate exists (D3), so an SPI capability for
it would reinstate the owner this design removes; and D2 deletes
`Backup::pitr_replay` (`crates/zeroship-data-core/src/storage.rs:603`)
along with its PostgreSQL implementation, which does nothing but
`INSERT INTO __zeroship_admin.pitr_targets` (`backend/postgres.rs:1727`), and
its SQLite stub (`zeroship-data-sqlite/src/lib.rs:2362`).

**`DbPlan` is defined by SC-3.** This document does not contain the grammar and
does not claim to. Its shared core and read family exist on disk as
`crates/zeroship-data-query-builder`, which depends on nothing and needs no database.

#### What the SPI must cover

`BackendHandle`'s 78 textual references are not 78 operational callers
(unverified). The operational set is `exec.rs` (route selection, publication),
`crud/mod.rs` (dialect selection, search dispatch, key/encryption dispatch),
`crud/read_pipeline.rs` (key resolution, decryption), `crud/mask_policy.rs`,
`crud/unmask.rs` (policy/row fetch, key, audit), `crud/mask_drift.rs`,
`transaction/mod.rs`, `cdc_lifecycle.rs`, `register_model/mod.rs` (deleted by
the cutover), and `drop_namespace.rs`.

**The SQL builders read the declared schema; nothing reads an introspected one.**
`runtime_schema_for` and its four production call sites went with the
introspection module. What survives is test-only: `*_for_tests` helpers at
`crud/mod.rs:2491` and `v8_classes/collection.rs:57`. That asymmetry is why the
cutover is smaller than a raw grep for schema readers suggests.

#### Module layout

A module split, not a crate split:

```text
crates/zeroship-plugin-db/src/
+-- frontend/         binding, runtime_descriptor, metadata, plan, ops/, v8/
+-- shared/           identifier, lease_key, sentinel_codec
`-- backend/          api, factory, postgres/*, sqlite/*
```

Rules: `frontend/*` never imports a concrete backend; the backends never import
each other; driver types never appear in `frontend/*`, `backend/api.rs`, or a
public signature; backend SQL lives only under its own directory;
`backend/factory.rs` is the only file naming both; no dialect match survives in
CRUD, metadata, V8, transaction or search code. Enforced by source gates and one
contract suite run against both factories.

**Every source gate above must first assert that the file it scopes to exists.**
None of `backend/api.rs`, `backend/factory.rs` or the `DbBackend` trait exists
today; the directory holds `mod.rs`, `postgres.rs`, `sqlite/` and
`lock_guard.rs`. A negative grep scoped to a path that was never created matches
nothing and reports success - so these gates pass on today's tree, pass if the
refactor is abandoned halfway, and pass if someone later deletes the boundary.
An existence assertion is one line and converts each of them from decorative to
load-bearing.

#### `zeroship-schema`'s retirement

The end state removes `zeroship-plugin-db`'s dependency on `zeroship-schema`.

**`zeroship-schema` has exactly one consumer, and `AGENTS.md` is wrong about
it.** The landing page's crate index says the crate is "reused by the migration
engine (write/diff) + plugin-db's data plane (read/introspect)". The first half
is stale: the manifests declaring `zeroship-schema` are its own `Cargo.toml` and
`crates/zeroship-plugin-db/Cargo.toml`, and that is all; source files
referencing `zeroship_schema` are only under `crates/zeroship-plugin-db`. The
in-sourced engine carries its own schema layer (`zeroship-migrate-backend`,
`zeroship-migrate-core/src/schema/`) and its own live introspection
(`crates/zeroship-migrate-postgres/src/backend/drift_sql.rs`). **That correction
must land in `AGENTS.md`**: a wrong line in the landing page gets repeated by
everyone who reads the landing page, and it has been.

*(One measurement trap: `grep -rl zeroship_schema` also matches
`crates/zeroship-control/tests/registry_schema_test.rs`. That hit is a test
function name, `registry_core_tables_live_in_zeroship_schema`, referring to the
PostgreSQL schema called `zeroship`, not to this crate. Spelling, not
behaviour.)*

Retirement is not a bullet point. The crate is **15,357 lines across 7 modules**
- `descriptors.rs`, `diff.rs`, `error.rs`, `ident.rs`, `lib.rs`,
`mask_codec.rs`, `query.rs`. `query.rs` alone is **12,104 lines**, which matters
because SC-3's justification quotes line ranges from a file roughly half this
size (`:2907-6011` for the runtime builders, `:1019-2905` for DDL rendering);
**those ranges no longer locate anything** and must be re-derived before they
are used to scope work. One of the crate's surfaces is security-critical:
`validate_collection`'s reserved-`__zeroship` prefix check
(`crates/zeroship-schema/src/query.rs:650-654`) is the sole guardian of the
namespace this design relies on. `diff::read_live_schema` and
`diff::estimate_row_count` (`crates/zeroship-schema/src/lib.rs:22-23`) lost
their only data-plane consumer when introspection was deleted. The other five
modules still have plugin-db callers that nobody has audited:

| module | references from `crates/zeroship-plugin-db/src` |
| --- | ---: |
| `mask_codec` | 6 |
| `error` | 6 |
| `diff` | 5 |
| `query` | 4 |
| `descriptors` | 3 |

(Counts unverified.) Retirement happens when SC-3's ledger reaches zero unported
entries **and** that audit is done, not when one inventory moves.

### 3.9 Operation context and total decode

```rust
pub struct DbOpContext { collection: CollectionHandle, tx_route: TxRoute, input: OwnedDbOpInput }
```

`OwnedDbOpInput` is produced by a **total, fallible** decode: every
`Object::get`/`Array::get_index` returning `None` aborts with
`INVALID_ARGUMENT` (a key is never skipped, `v8_bridge.rs:250`; an element is
never defaulted to null, `:227-228`); a pending V8 exception is re-thrown; each
key is read exactly once so a `Proxy` cannot substitute values after SDK
validation; per-array, per-object, node and byte budgets are enforced before
allocation; non-finite numbers, functions and symbols are rejected rather than
coerced to null, because a filter value becoming null changes the operator to
`IS NULL`.

This is tenant isolation, not robustness: before the fix, a throwing getter on a
filter key was silently dropped, so
`updateMany({tenantId: <throwing getter>, status})` could execute as
`WHERE status = ...` across every tenant's rows. **This has shipped**, with
`DecodeError` at `v8_bridge.rs:165`.

Pipeline: validate descriptor membership; singleflight lazy backend
initialization; `backend.prepare(request)` returning an `OpSession` (route plus
session setup); resolve the collection from the descriptor; build and execute on
the same route; decode/decrypt/mask/normalize; commit; emit success-only usage.

### 3.10 Resolved metadata and delivery paths

`ResolvedCollectionMetadata` carries the collection, the physical facts the
descriptor states, and per-field `SecurityDisposition`
(`VerifiedPlain` | `Encrypted` | `Masked` | `EncryptedAndMasked`).

Resolution is collection-wide: one invalid field rejects the collection before
any data SQL.

| Descriptor says | Result |
| --- | --- |
| Collection absent from the descriptor | `COLLECTION_NOT_DECLARED` before backend work |
| Collection declared, app database has no such relation | `SCHEMA_NOT_APPLIED` (raised today by the dev and SQLite paths for a genuinely absent app file) |
| Field declared plain | `VerifiedPlain` |
| Field declared encrypted / masked | The declared disposition, with the physical columns the `storage` block names |
| Field absent from the descriptor | Excluded from projection, decode and writes |

`None` must never mean both "verified plaintext" and "metadata unavailable" -
which is why `collection_schema` returns a typed error rather than an `Option`
(`descriptor.rs:58-65`).

**Delivery paths.** These rules bind every path returning row data. A change
event is resolved exactly as a read is: a `Masked` or `EncryptedAndMasked`
parent is replaced by its masked sibling and the sibling key dropped; an
`Encrypted` parent is dropped absent an authorization a read would honour.

Generated SELECT lists contain only declared logical fields plus required
platform fields; never `SELECT *`, never a physical-only column, and companion
columns are never creator-visible keys on any path.

**The mutation-side producer is suppressed in production, and any delivery
design must be written against the WAL consumer rather than against it.**
`is_app_suppressed(app_id)` gates the mutation-side publish with the comment
that when the WAL consumer runs for an app "it owns the publish path for events
this isolate writes" (`crates/zeroship-plugin-db/src/exec.rs:455-466` for the
reasoning; the call at `:501`). The real producer is
`wal_consumer::emit_for_tuple`
(`crates/zeroship-plugin-db/src/wal_consumer.rs:589`), which holds no operation
context. `publish` and `deliver_event` are synchronous functions with no
session, no pool and no `async` (`broker.rs:565`, `:745`, `:836`), so nothing
the delivery path needs may require a round trip. The WAL tuple carries no
platform metadata of its own (`broker.rs:80-115`). (Unverified.) Under the CDC
service the projection moves into the publication column list, so the excluded
bytes never reach the wire; the wire contract is that document's, not this one's.

The existing schema-pending window (`broker.rs:790-796`, guard at
`backend/mod.rs:1527`) has no production caller.

**Backends must distinguish "no such relation" from "relation present, not
enumerated"** wherever a relation is enumerated at all. The PostgreSQL attribute
scan restricts to `relkind = 'r'` (`crates/zeroship-schema/src/diff.rs:641`)
while the platform elsewhere models `'r','p','v','m','f'`
(`bootstrap.rs:1648`) and publishes `'p'` (`publication.rs:20-21`). A
partitioned creator table's parent is `relkind = 'p'`, so it is invisible to
that scan, and "no metadata" reads the same as "no protection needed". The data
plane no longer runs that scan, but `read_live_schema` still feeds the migration
engine's diff, so the rule is stated here and the defect is recorded as **L17**
in the defect register.

### 3.11 Mask policy, the ceiling, and key custody

`defineMaskPolicy` and the `Symbol.for("@zeroship/db/MaskPolicyState")` slot are
**deleted outright**. A policy any module in the isolate can write is not a
declaration, it is an input: creator top-level code runs before the bootstrap
drains that slot, the Rust side validates only the classification vocabulary,
and the result is persisted durably - surviving the isolate, the deploy, and
deletion of the offending code. It is reachable from any transitively bundled
package, and a second defeat exists via `@zeroship/db/internal`'s
`_flushPendingMaskPolicy`, dynamically importable by any referrer today. Every
pinned workflow isolate replaying an old deploy re-runs this boot
(`cache.rs:495` -> `:366`), so a deploy-pinned writer re-persists an old policy
over the current one.

**The replacement has two halves.**

- The **declared half is untrusted and deploy-scoped**: artifact data, with the
  same standing as the runtime descriptor, since the artifact is unsigned. It is
  declared in the creator's codebase, folded at build time, delivered through
  the artifact/init channel, and immutable for the isolate's life - the same
  path the descriptor already takes, through the same channel, validated at the
  same point. Its only security property is that it cannot outlive its deploy,
  which is the entire point of moving it out of durable storage.
- The **ceiling is worker configuration**, delivered at worker composition and
  fixed for the isolate's life.

Both are frozen into `DbIsolateBinding`, and effective permission is
`ceiling INTERSECT draft` computed **once at binding construction**.

**The meet is not a map intersection, and getting that wrong inverts revocation
for `auto`** - the actor with the most access. A ceiling that revokes `auto`
must deny `auto` even when the creator draft does not mention `auto` at all.
That failure is invisible to every same-key fixture, which is why the acceptance
arm in section 8 pairs it with a granted-path control.

**This deletes**, concretely:

- `defineMaskPolicy()` and the `Symbol.for("@zeroship/db/MaskPolicyState")` slot
  (`sdks/db/src/policy.ts`), and the `_flushPendingMaskPolicy` /
  `_peekPendingMaskPolicy` drain re-exported at `sdks/db/src/internal.ts:80-83`;
- the `zeroship.db.setMaskPolicy` native op
  (`crates/zeroship-plugin-db/src/v8_classes/db_platform.rs:145`);
- `dispatch_set_mask_policy` (`crates/zeroship-plugin-db/src/crud/mask_policy.rs:224`);
- the SQLite JSON sidecar - `mask_policies.json` under the backend's db
  directory, reached only by `persist_sqlite` / `load_sqlite`
  (`crates/zeroship-data-sqlite/src/lib.rs:207-211`,
  `crud/mask_policy.rs:350-358`);
- the broad `DbPlatform` V8 class, its private slot, `__zsDbPlatform`, and
  creator-facing replication diagnostics. `DbPlatform` exposes only
  `registerModel` and `setMaskPolicy` (`v8_classes/db_platform.rs:115`, `:145`),
  both of which this design deletes.

There is consequently **no `maskPolicyReady` promise and no readiness gate**.

**`ThreadDbContext::mask_policies` stops being a cache and becomes a field on
the binding**: no lazy load, no invalidation, no staleness, no write-through
refresh. The three properties a cache has to defend - what populates it, what
evicts it, what makes it wrong - cease to have referents rather than being
answered. That also closes a defect nobody had counted:
`mask_policies: HashMap<String, MaskPolicy>` is keyed by **`app_id` alone**
(`crates/zeroship-plugin-db/src/context.rs:368`, read at `:837-841`, written at
`:847-857`), so two pinned isolates of one app at two deploys share one entry
and the last boot to run wins for both.

**Precedent, and one difference in it that must not be copied by accident.**
`crates/zeroship-migrate-server/src/policy.rs` already implements
operator-ceiling meet creator-draft for migrations: the model at `:1-25`, the
monorepo-owned CONFINED default ceiling as a TOML document compiled in via
`include_str!` at `:48-59`, and the compose at `:119-122`. Masking should look
like its neighbour rather than invent a second shape. But that compose is
**escalation-reject** - "a draft grant looser than the ceiling permits is
rejected, never clamped" (`policy.rs:16-17`, `:119-121`) - while masking's meet
**clamps**. Both are defensible and they are not interchangeable: reject
surfaces the creator's mistake at deploy time, clamp lets a deploy succeed with
less access than it asked for. That ceiling is also **DDL-knobs-only**: its key
set is `CREATE TABLE` / `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS
(`policy.rs:32-35`), with no vocabulary for mask classifications, so sharing the
store would put two unrelated policies under one name. And its staleness
response is `ApprovalStaleCeiling` -> "re-submit required"
(`crates/zeroship-migrate-server/src/apply.rs:214-221`), which is right for a
migration awaiting approval and wrong here.

**Costs, stated because a section listing only benefits is not finished:**

- **Changing a mask policy requires a build and a deploy.** No runtime edit, no
  dashboard toggle, no hot path to a looser rule during an incident.
- **Revocation latency becomes worker-roll time.** An operator who lowers a
  ceiling has not changed anything until the workers carrying the old value are
  gone. The bound is a deployment property, not a millisecond one.
- **Deploy-pinned workflow isolates keep both old halves until evicted.** They
  are pinned by construction (`PinnedWorkflowKey { app_id, deploy_hash }`,
  `crates/zeroship-worker/src/cache.rs:28-32`) so that workflow replay sees the deploy it
  was recorded against. The set is bounded by `max_pinned_isolates_per_app`, so
  the exposure is bounded, and **force-eviction is the single lever** for
  immediacy - not one lever among several.
- **The dev tier needs a named ceiling source.** `zeroship serve` and the Vite
  dev vector are separate composition points from the worker, and "worker
  configuration" does not say what they read. SC-4 and SC-5 do not cover it.

#### Key custody

One key per app, derived `HKDF(platform_master_key, app_id, key_version)`. There
is no key table and no getter. The platform master key is already operator
config with a validated floor (`crates/zeroship-core/src/config/secrets.rs`:
`platform_secret` at `:133`, `validate_master_key_material` at `:322`,
`decoded_master_key_len` at `:289`, and the `PLATFORM_SECRETS` table at
`:88-129`).

**The reason is cryptographic, and the ordering matters.**
`canonical_aad(collection, column, row_pk_bytes)`
(`crates/zeroship-data-core/src/encryption/aad.rs:75`) already binds domain
separation more tightly than per-column keys ever did: it separates *rows*,
which keys never did at any granularity, and it authenticates the column name,
which is the property a per-column key was reaching for. It also binds the wire
version FIRST, so the version byte - unauthenticated framing in the envelope -
is covered by the AEAD tag (`aad.rs:87-93`). A per-column key adds nothing on
top of a per-row, column-authenticated AAD.

Storing a root key in the tenant's own database also puts the key beside the
ciphertext it decrypts. `SECURITY DEFINER` stops the tenant *querying* it; it
does nothing against possession of a dump, a backup, a physical replica, or a
PITR restore, all of which carry both halves in one artifact. The boundary was a
query-time one and the threat is a bytes-at-rest one.

**What was deployed was already one root for everything.** `key_id` defaults to
the literal `"default"` in both producers
(`crates/zeroship-schema/src/query.rs:2295`, `diff.rs:1636`), and `derive_key`
already salts the root by `app_id`
(`crates/zeroship-data-core/src/encryption/keys.rs:427`). Per-column keying was
nominal; this decision deletes a table that was not providing separation, not
the separation itself.

**What is lost: rotating one column without touching its siblings.** Rotation
survives at app granularity - **and its lazy-re-encryption-on-write half is not
implementable today.** The key version has no per-row carrier. `key_id` lives in
the column's stored sentinel `zsenc:<mode>:<keyId>:<wraps>`, built at
`crates/zeroship-schema/src/mask_codec.rs:88-95` and parsed at `:122-153`, so a
column names exactly one key version at a time; and the ciphertext envelope's
leading byte is the **wire-format** version, not a key version, with `unpack`
rejecting anything but `0x01`
(`crates/zeroship-data-core/src/encryption/wire.rs:83-88`). The insertion point
is already identified in the code: `wire.rs:7-17` reserves the leading byte so a
future shape "requires no in-place data migration", and `aad.rs:84-89` commits
to threading the version through `canonical_aad` as a parameter when `0x02`
ships. A key version in that header **must** also be bound into the AAD, or a
downgrade to an older key version is not tag-detectable. This is named, not
solved.

#### The AAD binds the physical column, and there is no `aadColumn`

The AEAD binds the physical column the descriptor already records. A separate
`storage.aadColumn` field would exist only to keep pre-flip rows decryptable -
rows that do not exist - which is the shape the no-back-compat rule forbids.

**The surviving constraint is that moving an encrypted value is a re-encrypt,
not a rename.** `canonical_aad` (`encryption/aad.rs:75-78`) feeds
`(collection, column, row_pk_bytes)` through `extend_with_len` into the AEAD's
additional data, after binding the wire version first (`aad.rs:88-95`). The
column name is **authenticated**, not merely used to look the value up, so a
migration implementing a column move as `ALTER TABLE ... RENAME COLUMN` produces
a table whose every encrypted cell fails tag verification. With no deployed
ciphertext that costs nothing today and cannot be made to cost nothing later,
which is the whole argument for settling it now.

Production call sites of `canonical_aad` are **five**, all in plugin-db:
`crud/mask_drift.rs:570`, `crud/encryption_pass.rs:200` and `:337`,
`crud/mask_backfill.rs:154`, `crud/unmask.rs:457`. The two further occurrences in
`crud/write_pipeline.rs` (`:743`, `:951`) that pass a hardcoded `"ssn"` sit below
the `#[cfg(test)]` at `:628` - worth stating, because a hardcoded column name in
the write pipeline would be a defect and it is not one. (Unverified.)

### 3.12 Transactions and connections

Transaction views enumerate collections from the isolate's descriptor and clone
its binding. Each collection carries the exact transaction route alongside the
same identity and descriptor as its parent.

Everything else - states, transitions, health, ownership, cancellation,
deadline, savepoint frames, effect buffer, terminal outcomes, and the admission
key - is **SC-1**. This document does not restate five labels as though they
were a protocol.

Two constraints SC-1 must satisfy, both from current behaviour: settlement must
never interpret an absent client as proof that terminal SQL ran
(`transaction/mod.rs:986-997` does today), and cancellation must not drop the
client between `take` and the manual restore (`exec.rs:188-201`).

Randomized-encryption atomicity is in scope and depends on SC-3's plan variants:
establish the conflict winner's stored id atomically before encryption, preserve
`predicate AND id` in the final mutation, and run the multi-row algorithm in one
internal transaction so behaviour does not change with encryption mode.

**Connections always come from a pool (D10), and that is a capacity-model change
rather than a refactor.**

**THIS PARAGRAPH'S PREMISE WAS MARKED `unverified` AND HAS NOW BEEN VERIFIED
FALSE.** It read: `acquire_dedicated_client` opens a brand new TCP connection per
transaction via `compio_postgres::connect` directly, plus a detached task per
connection, and never touches the pool - leaving the concurrent-transaction
ceiling *unbounded*. Measured 2026-09-02 at
`crates/zeroship-data-postgres/src/postgres.rs:328`, the whole body is
`self.pool.get_owned().await`, whose error arm goes out of its way to
distinguish "this worker hit its own pool ceiling" from "the server is
unreachable". D10 therefore SHIPPED: the checkout is pooled and the ceiling is
the pool's size, not unbounded. What remains live below is the *sizing*
question - what that ceiling should be - not the introduction of one.

The original reasoning is kept because the sizing argument still rests on it: a
worker multiplexing ~200 isolates per OS thread
(`crates/zeroship-plugin-db/src/exec.rs:1321`) that each open a transaction opens
~200 connections.

Three sites create connections today and only one is a pool checkout:

- `crates/zeroship-plugin-db/src/lib.rs:998` - `Pool::connect(&url, 8)`, the
  shared data pool. This one is already right.
- `crates/zeroship-plugin-db/src/lib.rs:698` - `Pool::connect(url, 2)` **per
  deprovisioned app**: a new pool per deletion, paying two connects, two
  authentications and two TLS handshakes for what should be a checkout from a
  long-lived platform-role pool. Under D10 this changes.
- `crates/zeroship-plugin-db/src/wal_consumer.rs:368` -
  `repl::connect_replication(...)`, a dedicated non-pooled connection. **This is
  a genuine exception**: a logical replication session is opened with the
  `replication=database` startup parameter and stays in streaming protocol mode
  for its whole life, so there is nothing for a pool to multiplex. The rule for
  it is not "pool it" but **bound and account for it**; under L12 that
  connection moves to the CDC service and becomes O(1) per cluster.

**The inversion D10 buys, and what it still owes.** Before, one app could
exhaust `max_connections` and take the cluster down for every tenant everywhere;
after, one app can occupy the pool and stall its co-residents on one worker. The
second is much better; it is not nothing. Three questions remain unanswered and
must be answered before the change lands:

- what the transaction ceiling is, given the pool holds 8 and is now shared
  across every app on the worker rather than per-app;
- what a transaction does when the pool is exhausted - queue behind the SC-1
  deadline, or refuse with a typed error. Queueing is preferable only while the
  deadline is shorter than the caller's patience, or a fast failure has been
  converted into a slow one;
- whether one app can starve others. Nothing currently stops one app holding all
  8. This is a **new** cross-tenant failure mode created by a change made for
  safety, and it wants an explicit mitigation - a per-app checkout cap, or fair
  queueing - rather than being discovered when one app's slow transactions stall
  its neighbours.

### 3.13 SQLite and the dev tier

The explicit migration path remains the schema authority; runtime boot applies
nothing.

**There is no per-operation cross-process flock lease.** An exclusive flock
survives for **restore's file swap only** - the one thing WAL does not cover,
since a lock release must not leave a connection bound to an obsolete inode. The
`:memory:` process-local guard is retained.

The actor is redesigned per **SC-2**, because the current one cannot honour the
cancellation and RAII contract this design depends on: commands carry only SQL
plus reply channels and still run after the receiver is dropped
(`session.rs:155-245`), one FIFO loop runs every route on one connection
(`:403-445`), and all handles clone that same actor (`:661-675`).

SC-2 deliberately changes two documented, creator-visible behaviours: an app's
autocommit **reads** proceed while it holds an open explicit transaction
(retiring the `tx_route.rs:119-124` divergence and the
`docs/reference/sqlite-divergences.md` entry that records it), and cancellation
begins interrupting a running statement rather than only inter-statement gaps.
The acceptance arm is **reads**, not ops - SQLite has one writer per database on
any number of connections, so an "ops" arm cannot pass.

App attachment moves entirely into session preparation, reached by every path
that can touch an app file. `ensure_attached` validates app identity and
canonical path and opens the existing file with no-create semantics; a missing
file returns `SCHEMA_NOT_APPLIED` and a test proves no empty file was created.

Every path inserted into a SQLite `file:` URI is **percent-encoded**; SQL-quote
escaping is not sufficient (`session.rs:635` escapes for a string literal then
drops the path into a URI), so `?`, `#` or `%` in `db_dir` are parsed by
SQLite's URI parser and `?mode=rw` after an existing `?` is not the mode
parameter. Path vectors include one `db_dir` of each kind, each asserting the
resolved file and that `mode=rw` arrived as the mode parameter.

**A non-SQLite dev URL is typed-rejected**, not implemented (SC-4). Today it is
neither: the dev command always derives SQLite paths regardless of scheme
(`migrate-dev.ts:116-131`) and the addon exposes only `applyIrSqlite`, so a
PostgreSQL `DATABASE_URL` gets its migrations applied to a SQLite file while the
runtime is pointed at PostgreSQL.

### 3.14 Caches and bounds

With no per-operation introspection there is no live-metadata cache to bound.
**The requirement did not go away with it**, and the cache it now applies to is
the one that was always the worst:

**The encryption `KeyStore` is unbounded and on the hot path.** Its module
documentation states the property outright: "once a `(app_id, key_id)` entry is
inserted, it stays for the lifetime of the `KeyStore`. There is no rotation
surface today" (`crates/zeroship-data-core/src/encryption/keys.rs:48-55`). It
holds tenant key material for every encrypted app the thread has ever served,
and the `KeyStore` belongs to the backend, which lives in the thread-local
context until backend reset or thread exit - **not** isolate eviction.

**And it is a hot-path allocator, not just a leak.** Key resolution runs per
encrypted column per returned row on reads and per encrypted field on writes:
`backend.resolve_key(app_id, &key_id).await?` sits **inside** the
`for ... in to_decrypt` loop of a function invoked once per row
(`crud/encryption_pass.rs:335-336`). The lookup is
`cache.get(&(app_id.to_string(), key_id.to_string()))` (`keys.rs:316-329`) -
**two `String` allocations on every call, including cache hits**, because the
key is an owned tuple that `get` cannot borrow into.

The fix is unglamorous and certain: resolve the distinct `(app, key)` set once
per operation rather than per cell, make the lookup borrow instead of allocate
(a nested map, or a key type implementing `Borrow` for the `(&str, &str)` pair),
and bound the cache with zeroizing eviction keyed on the full identity.

**Three rules this design commits to for every per-app cache that remains:**

1. **A bound stated as a number**, the way a retry policy is, not the word
   "bounded" - and **bound BYTES, or bound entries AND cap per-entry column
   count.** An entry-count bound alone does not bound: measured per-entry cost
   varies about 25x across realistic shapes, because it depends on facets rather
   than column count.
2. **An eviction policy whose key includes the full identity**, so evicting is
   never confused with invalidating.
3. **Per-app state is stored hierarchically, behind `Rc`, resolved once per
   operation and threaded through** - not keyed by string concatenation into one
   flat thread-global map, and not `clone()`d per stage. A hierarchical
   `app_id -> {collection -> schema}` map answers the name query in one hash
   lookup, makes eviction-by-app a single removal instead of a prefix sweep, and
   removes a `format!` allocation per call.

**Isolate eviction is a pruning hint, not the bound.** A metadata entry is
kilobytes; a V8 isolate is orders of magnitude larger, so metadata entries
should **outnumber** isolate entries under an independent, larger, byte-capped
bound. `max_isolates` defaults to 200 per thread
(`crates/zeroship-worker/src/config.rs:129`, unverified), and under LRU churn -
and under CHWBL spill oscillation - evict-then-reload is the common case at
target scale, so coupling metadata lifetime 1:1 to isolate lifetime would re-buy
the cost it removes. Mechanically the hint IS deliverable for the LRU arm, since
`evict_lru` runs on the owning thread, the same thread as the DB context; the
deprovision arm is not, and today no eviction path calls into plugin-db at all -
the only reference to plugin-db anywhere in `crates/zeroship-worker/src/cache.rs`
is the `DbPlugin::new` construction at `:219`.

**The plugin set is memoised per thread, not process-wide**, which is why a
process-wide cache currently has no owner: `build_runtime` calls `plugin_set()`
(`cache.rs:426`), which caches `create_plugins()` into a `thread_local!`
(`:194-204`). SC-5 owns the move.

### 3.15 Error contract

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
being the sole authority: every condition that was worth retrying was a
condition of a read the data plane no longer performs.

**`APP_DEPROVISIONED` and `STALE_APP_INCARNATION` are separate on purpose,
because they occur at different times for the same handle.** Deprovision leaves
the incarnation in place and sets a tombstone, so a handle carrying A first
meets a tombstone bearing *its own* incarnation - that is `APP_DEPROVISIONED`.
Only once the app is re-provisioned, and the record holds B, does the same
handle fail on mismatch. Collapsing them would make the audit trail unable to
distinguish "the app is gone" from "the app came back without you".

All three of Fork C's codes are non-retryable. That is the point of a terminal
denial: none of them is improved by trying again.

**None of these codes may be argued from as though it described shipped
behaviour.** Of the seven, exactly one - `INVALID_ARGUMENT`, 3 occurrences in
`crates/zeroship-plugin-db/src/` - exists in the codebase; the rest are proposed
surface with zero occurrences. `collection_not_declared` exists as a *string*
raised by `descriptor.rs:71`, which is the code arriving ahead of the contract
rather than the contract describing the code.

---

## 4. Invariants

1. **Creator code cannot mutate runtime DB metadata**, and cannot supply a
   security policy as an input.
2. **One isolate has one declared descriptor, one declared mask policy, and one
   operator ceiling**, all fixed for its lifetime. All three arrive before the
   isolate exists, none is reloadable, none is cached, so none can be stale.
   The argument is one argument: a value creator code can influence after boot
   is an input, not a declaration, and that argument never had anything to do
   with schema in particular.
3. **One operation uses one metadata snapshot**, binding every path that returns
   row data, including CDC and live-query delivery. An immutable descriptor
   makes this trivially true, and it is kept stated for that reason: the
   cheapest way to reintroduce the bug is to add a second source of metadata and
   not notice the invariant no longer holds.
4. **Security metadata fails closed.** A missing, invalid or unparseable
   descriptor prevents data SQL. Boot fails; no operation runs against metadata
   that could not be understood. **What this does not cover is a descriptor that
   is well-formed and wrong about the database** - at runtime nothing covers
   that, which is why D12 puts the check in the deploy pipeline and the flip
   behind it.
5. **No data-plane path executes DDL (D11).** Schema change belongs to
   `zeroship-migrate`; a runtime that can alter schema is a runtime that can
   disagree with the descriptor describing it. This is enforced by *deleting*
   the DDL-emitting paths, not by a classifier that may never traverse them.

   **DONE - this step shipped, and the paragraph below is kept in the past
   tense rather than rewritten.** The live index-creation surface WAS two
   statements in the PostgreSQL backend: `CREATE INDEX CONCURRENTLY ... USING
   ivfflat` (DELETED, was `crates/zeroship-plugin-db/src/backend/postgres.rs:518`,
   from `ensure_vector_index`) and `... USING GIST` (DELETED, was `:668`, from
   `ensure_spatial_index`). They moved into the migration path, so a `.vector()`
   or `.spatial()` field now declares its index the way every other index is
   declared, and `ensure_vector_index`, `ensure_spatial_index`,
   `create_index_with_recovery` and `create_index_with_recovery_audited` were
   DELETED. Re-measured 2026-09-02: zero definitions and zero call sites
   (`grep -rn 'fn ensure_vector_index\|fn ensure_spatial_index\|fn
   create_index_with_recovery' crates/ libs/` matches nothing). The names DO
   still appear about ten times in `zeroship-migrate-core` and two test files,
   every one a comment naming the deleted function to say which engine-side
   renderer replaced it - so a bare name grep reports them as live and is the
   wrong instrument here. The backend that carried them is now
   `crates/zeroship-data-postgres/src/postgres.rs`, and what survives there is
   only the two `pg_extension` capability PROBES (`:527` for pgvector, `:625`
   for PostGIS), which read a catalog rather than emitting DDL.

   **`__zeroship_migrations` is not a migration record; it is the provenance log
   for exactly that DDL**, written only from `create_index_with_recovery_audited`.
   Once the index creation leaves, nothing writes it, and `audit.rs` goes with it
   together with the `Backend` methods `ensure_audit_table`, `write_audit_row`
   and `next_schema_version` and their SQLite arms. Deleting the table before the
   writer would keep the writer and drop its provenance, which is what
   `audit.rs:20-42` exists to guarantee - so the order is index first, table
   second. Removing the name from creator schemas also frees it for the engine
   journal.

   The `CREATE TABLE IF NOT EXISTS` statements that go with the DDL exit, all in
   `crates/zeroship-plugin-db/src/`, are six over three tables:

   | table | PostgreSQL | SQLite |
   | --- | --- | --- |
   | `"<app>"."__zeroship_migrations"` | `audit.rs:236` | `zeroship-data-sqlite/src/lib.rs:1319` |
   | `"<app>"."__zeroship_audit_mask_drift"` | `crud/mask_drift.rs:793` | `crud/mask_drift.rs:827` |
   | `"<app>"."__zeroship_audit_unmask"` | `crud/unmask.rs:850` | `crud/unmask.rs:901` |

   The unmask table is created from `ensure_audit_unmask_table`
   (`crud/unmask.rs:838`), called at `:732` inside `write_audit_unmask_row`,
   which is reached from the **denied** path (`:405`), the granted one (`:428`),
   and two further callers (`:1217`, `:1446`). Audit insertion survives as an
   insert-only SPI capability (3.8); the table it writes is provisioned by the
   migration service, never lazily by the data plane.
6. **Raw JavaScript does not mean unverified plaintext.**
7. **No creator-reachable platform capability exists.**
8. **A private module is invisible, not allowlisted.** Secrecy of a specifier is
   never a security boundary.

---

## 5. What is implemented

On `feat/dbbind-impl`. Each landed fix carries a regression test proved red
under a verified-applied mutation unless the entry says otherwise.

| Commit | What landed |
| --- | --- |
| `632c1d1fa` | **The descriptor becomes the sole schema authority (D6, D7).** `crud/introspect_schema.rs` (1,054 lines) and `live_metadata.rs` (517) deleted, replaced by a 129-line descriptor reader. 31 files, +2219/-3044 |
| `4c8e84134` **(dangling)** | Runtime schema metadata keyed by `DbBinding { app_id, deploy_token }`; `IsolateDbContext` renamed `ThreadDbContext` (L10) |
| `5b9bcbd49` **(dangling)** | The savepoint frame-effect fate |
| `92bd67633` | `POSTGRES_MAX_BIND_PARAMETERS` / `SQLITE_MAX_BIND_PARAMETERS` as separate per-dialect constants (L14) |
| `d49930f34` | `updateMany` runs inside `AtomicWriteFrame` and refuses over `MAX_QUERY_LIMIT` (L15) |
| `f44bcf6b6` **(dangling)** | A per-populate admission cap and a per-key singleflight on cold misses (L18's two landable halves) |
| `72a3dc04b` **(dangling)** | The meter's unbounded growth (L20's first half). The stall itself was deliberately left alone |
| `8c6caa465` | The dev-mode SSRF bypass: `ZEROSHIP_DEV=1` inherited by a worker disabled SSRF for every `fetch`, the DNS resolver, and the egress floor (L23) |
| `b801bf12b` | `unmaskField` on PostgreSQL, which had never worked in a shipped binary (L25) |
| `a0074e154` | `distributed_live` on `main` (L27) |
| `fc2c889db` **(dangling)** | The racing test suite |
| `61298897b`, merged `93bc20126` | Full-text search deleted in the three layers that still advertised it, with no producer anywhere (L11) |
| `d92efa740` | All 35 platform migrations rewritten from `up()` to the `schema()` form, **DDL-neutral and measured**: IR envelopes byte-identical across all 35 (md5 `d37a1bb7156c6e75af8a0f939c7333eb`, 659 ops), rendered SQL identical for 33, and a one-variable control - the 35 originals through the new glue - refusing 35/35 |
| `2690b5a16` | `zero-migrate baseline`, the verb that lets the rewritten corpus be adopted by a database that already holds its schema |
| `a91b690d6` | Adoption refuses a database the corpus did not produce, diffing full snapshots rather than table names |

Also landed, from step 2: total V8 decode with `DecodeError` (`v8_bridge.rs:165`)
and the `COMMIT`-answered-`ROLLBACK` command-tag check in `exec_terminal_on_tx`
(`transaction/mod.rs:139`).

**Six of these SHAs are not on this branch.** Each of `4c8e84134`, `5b9bcbd49`,
`72a3dc04b`, `fc2c889db` and `f44bcf6b6` - and `22c4d75f1`, cited in SC-5 for
the plugin-set fix - resolves to a commit under `git cat-file -t` and fails
`git merge-base --is-ancestor <sha> HEAD`. **Their content is present**: L10's
rename is in the tree, with `ThreadDbContext` occurring 45 times in
`crates/zeroship-plugin-db/src/context.rs` and zero `IsolateDbContext` residue
anywhere under `crates/zeroship-plugin-db/src/`. The work reached this branch
under different hashes.

Two rules follow, and both have already been paid for once. A dangling commit
reads perfectly, so **reading a commit does not tell you whether it is in the
tree** - `git merge-base --is-ancestor` does. And every SHA in this table
answers "which commit landed this" and never "is this fix in the tree"; the
second question is answered by looking at the tree.

**Steps 3 and 5a are recorded as landed on the branch** - `OwnedPooledClient`
plus the SQLite actor's reservation/cancel/terminal protocol, and `DbService`
ownership - each reviewed adversarially with every finding closed. **No commit
SHA is recorded for either**, so unlike the table above they cannot be checked
with `git merge-base --is-ancestor`. Unverified; the SHAs are owed.

**What is exactly zero:** the private module map, the artifact/init channel, the
`DbIsolateBinding` itself, the mask-policy artifact wire, the operator ceiling as
worker configuration, Fork C's identity substrate, `DbPlan`'s remaining families
and its ledger, and the SC-4 dev mechanism.

**THIS LIST CARRIED TWO ITEMS THAT HAD ALREADY SHIPPED, UNTIL 2026-09-01.** Both
are removed above; both are recorded here rather than silently dropped, because
the way each survived is the instructive part.

*The deletion of `registerModel`* sat in the zero list while **the very next
sentence of this same paragraph said it was deleted**. A contradiction inside one
paragraph is not a stale citation - nothing external moved - it is a list edited
without re-reading the prose beside it. Re-verified 2026-09-01:
`crates/zeroship-plugin-db/src/register_model/` does not exist and `registerModel`
occurs zero times in any `.rs` file.

*The masking storage flip* was listed as zero while it is implemented and covered
end to end: `raw_column_name` (`crates/zeroship-schema/src/query.rs:2218`) is the
live split, and `crates/zeroship-plugin-db/tests/mask_flip.rs` exercises it
against a real PostgreSQL - 9 passed / 0 failed, measured 2026-09-01 against
PostgreSQL 18.6. Nothing in the tree reported this; the entry simply outlived the
work.

The rule this pays for: **a "what is zero" list decays in the opposite direction
from a citation.** A stale citation points at something that moved and can be
caught by a path check, which is why `tests/doc_citation_gate.sh` exists and is
green. A stale zero-list entry points at nothing at all, so no gate can see it,
and it reads as a to-do that someone will dutifully re-do. Check this list
against the tree before trusting any entry.

`v8_classes/db_platform.rs`'s `setMaskPolicy` (`:145`) is still live, and the
descriptor entries `registerModel` used to publish are now planted natively at
boot.
That list names mechanisms, not documents: `DbPlan`'s shared core and read family
exist as `crates/zeroship-data-query-builder`, and SC-5's service ownership partly landed
as step 5a while SC-5 as a contract is unimplemented.

**One coverage gap opened by `632c1d1fa`, stated because it is invisible from a
green suite.** `distributed_live` was the tree's only live boot of a V8 isolate
with `env.db` registered and no descriptor. Fixing it to ship a descriptor -
which is correct, and is what `docs/reference/zeroship-standard.md` prescribes -
means **nothing now observes `collection_not_declared` end to end through a real
isolate against a real PostgreSQL.** That path is covered only by unit tests
(`descriptor.rs:92-101`, `crates/zeroship-worker/src/sync.rs:820`, and
`sdks/bootstrap/tests/dev-entry-descriptor.test.ts`). Getting it back is a
**second test**, not a loosening of the first.

---

## 6. What is open, and what blocks each

### Decided: L12, the replication-slot ceiling, is a new CDC service

Live subscriptions cost one PostgreSQL logical replication slot per
`(app x worker)`, against a server-wide ceiling of 10 on a restart-only GUC,
where each slot is also a walsender competing for `max_connections`. The ceiling
is the weaker half of the argument: slots on one database do not partition
decoding work, they replicate it - measured on pg16, five slots created at one
LSN, one workload of 40,000 rows producing 68MB of WAL, each slot decoded in
turn, total 1,335ms against 309ms for one slot, a ratio of 4.32 out of a
possible 5.00, each slot decoding the same 40,002 changes. Ten subscribed apps
on one database pay ten full decodes of every transaction and up to 640MB of
decode buffers (`logical_decoding_work_mem` 64MB per slot). The cost is
`O(apps x total_WAL)` where the information is `O(total_WAL)`.

The decisive argument is not slot count but a credential: consuming a logical
slot requires `REPLICATION` on the connecting role, there is no narrower grant,
and this platform grants it - with `BYPASSRLS` - to the login role of the
process that executes creator code
(`db/migrations-ts/20260818000200_worker_database_authority.ts:35`). That is a
present-tense violation of D5, and only moving WAL consumption into a process
that runs no creator code lets `zeroship_worker` drop both attributes.

**A dedicated CDC service owning O(1) slots is being built**, specified in
`docs/proposals/2026-08-28-cdc-service.md`. This design set hands it three
requirements it must carry: a wire projection that is a whitelist over declared
fields (including the broker's `changed_columns`), a fixture for the mask-only
shape, and the schema-change signal that decisions D6/D7 left subscribers
without.

**What this leaves blocked in SC-3:** the relation family's live-query lowering
and the effects family (the publication a committed mutation owes the broker)
follow the service's wire contract. The shared normative core, read, write,
search and unmask families have no transport dependency, and the existence proof
is `crates/zeroship-data-query-builder`.

### Open: Fork C's identity state has no home

`incarnation`, `deprovisioned_at`, the tombstone rule and the authority domain
lived in a platform-schema row. **None of these is schema metadata**, and
deleting the schema removed their storage without removing their requirement.
Startup DDL validation would not answer the question even if it were not
deferred: two incarnations of one app id have the *same* schema, so validation
passes for both.

The control plane is the obvious candidate, since app lifecycle already lives
there, but that is not this document's call. Any answer must preserve four
properties, which are the ones the deleted design paid for:

- a **durable tombstone** that is never deleted, so a deprovisioned id stays
  deprovisioned;
- a **CAS against an expected incarnation**, so a delayed cleanup naming A
  against a record holding B matches nothing and refuses. This is not
  hypothetical: the worker's pending-deprovision set stores bare UUIDs and
  deprovisions by app id alone
  (`crates/zeroship-worker/src/sync.rs:135-166`), and SC-5 requires a cleanup
  carrying incarnation A not to act on B;
- an **authority domain a restored dump cannot assert about itself** - the
  reason it was never a column;
- the three error codes staying distinguishable.

**Until this is decided, Fork C is specified and unhomed, and the identity
substrate step cannot be written.**

### Decided and blocked: the masking storage flip

Under D12 the flip - `ssn` holds the masked value, `ssn_raw` holds the real one -
lands as the second line of defence behind the deploy-ordering precondition. Its
value is precise and worth stating, because it is the only mechanism that covers
the one failure with no runtime check: `read_pipeline::apply` runs the mask pass
only `if schema_has_masked_columns(&schema)`, and that predicate reads the
descriptor (`crud/mod.rs:2590-2602`), so a descriptor that has not caught up
returns the parent column untouched - plaintext today, and the mask post-flip.

**It is not in the tree.** `mask_sibling_column_for_field` still returns
`format!("{field}_masked")` in both copies
(`crates/zeroship-migrate-backend/src/schema.rs:557`,
`crates/zeroship-schema/src/query.rs:2151`) and nothing anywhere spells `_raw`.

**It must not be implemented until its write path is guarded.** SC-6 owns the
blocking list and `docs/reviews/2026-08-28-flip-write-path.md` is the
specification: three silent write-correctness inversions (upsert duplicate-insert
and upsert clobber; live-query subscriptions stop firing; unique constraints
enforce the wrong thing), one data-loss hazard (two passes writing one key with
no ordering contract), one AEAD invariant held by convention, a swap of which
column carries the declared type and the whole constraint set - which the
migration engine's differ is structurally blind to - and the loss of
equality-by-real-value until the keyed lookup column
(`docs/reviews/2026-08-27-query-by-plaintext.md`) ships.

**Much of that specification is not flip work**, and those parts should land
whether or not the flip does. `RETURNING *` returns plaintext under `ssn`
**today**; the PostgreSQL introspector discards sentinels silently **today**;
`rawProjectable` describes a gate nothing can enforce **today**. Three facts
about the current code bound that work:

1. **Twelve SQL-emitting `RETURNING *` sites** in
   `crates/zeroship-schema/src/query.rs` (12,104 lines; `mod tests` begins at
   `:6035`): `:3584` (insert), `:4005` (updateOne), `:4157` (insertMany),
   `:4228` (updateMany), `:4254` (deleteMany), `:4290` (deleteOne), `:4445`
   `:4483` `:4525` `:4557` (soft-delete and restore, one and many), `:5937`
   (upsert), `:6020` (findOrCreate). Twenty *functions* reach those twelve
   sites, because six are thin delegating wrappers.
2. **Nothing on the production read path removes an unknown key from a returned
   row.** The only key removal is `mask_pass::wrap_row_on_read`, which removes
   exactly `format!("{col}_masked")` (`crud/mask_pass.rs:469`, `:480-482`), so a
   raw column survives to `mapResultDoc` (`sdks/db/src/utils.ts:28-33`). And
   `decrypt_row_on_read` gates decryption on the same hardcoded sibling name
   (`crud/encryption_pass.rs:295-301`), so post-flip an encrypted+masked field
   would skip decryption entirely and reach JS as base64 ciphertext, while a
   mask-only field reaches JS as plaintext. (`strip_encryption_markers` is not
   this path: it is `#[cfg(any(test, feature = "test-helpers"))]` at
   `crud/encryption_pass.rs:501` and strips `__zsbin__` markers from a *write*
   document before binding.)
3. **A `__zsmask:` sentinel on a column not ending `_masked` is discarded with
   no warning.** `read_live_schema` filters on the suffix at
   `crates/zeroship-schema/src/diff.rs:671`
   (`if comment.starts_with("__zsmask:") && column.ends_with("_masked")`) and
   strips it at `:716`; a non-matching column falls through both `if`s while the
   malformed-sentinel arm ten lines below warns loudly (`:737-744`). That arm is
   production and feeds the migration engine's diff. The SQLite equivalent,
   `parse_mask_sentinels` (`zeroship-data-sqlite/src/lib.rs:2232`) and its only caller the
   `SchemaIntrospect for SqliteBackend` impl (`:804`), are both
   `#[cfg(any(test, feature = "test-helpers"))]` and the dev tier never runs
   them.

Three further items the flip owes: lookup by real value must survive or the
feature is closed rather than secured; constraints and indexes must follow the
raw column, except that `.unique()` on a randomised-encrypted field must stay
refused at declare time (`sdks/db/src/types.ts:1153-1160`) because ciphertext
equality enforces nothing; and the creator-visible behaviour change owes
`docs/reference/db.md` an entry beside the mask kinds.

### Open: deterministic encryption mode (D9)

Kept, not built out, not deleted. **It is unreachable from the authoring
surface**, which is the only surface a creator has: `ColType::Encrypted { of }`
(`crates/zeroship-migrate-ir/src/ir.rs:670`) carries the inner type and nothing
else - its own doc says "Migration-first authoring supports default-mode
encryption only" - and lowering **hardcodes** `"mode": "randomised"` at
`crates/zeroship-migrate-core/src/render/lower.rs:9514`. Since the committed
migration set is the schema source of truth
(`docs/reference/zeroship-standard.md`), this is not a feature missing its query
half; it is a code path no creator input can reach.

**The crypto half IS built**, which is what makes the wrong reading plausible:
`encrypt_deterministic`
(`crates/zeroship-data-core/src/encryption/aead.rs:92`) derives a synthetic
nonce as `HMAC-SHA256(k_siv, aad || plaintext)`, so identical plaintext yields
byte-identical output, with tests pinning it. The mode also has real runtime -
it selects the AAD shape, dropping `row_pk` (`aad.rs:75-78`,
`backend/postgres.rs:1173`, `zeroship-data-sqlite/src/lib.rs:2074`,
`crud/mask_drift.rs:575`, `crud/unmask.rs:234`).

The query-by-plaintext design
(`docs/reviews/2026-08-27-query-by-plaintext.md`) argues a keyed lookup column
serves **randomised** mode - the only reachable one - and therefore closes this
question rather than reopening it. If the mode is still unbuilt when the filter
path is audited under L9, that audit should decide it rather than route around
it.

### Open: the ceiling's configuration source

D4 makes the ceiling worker configuration. **Its format, its configuration
source, and what the dev and `zeroship serve` vectors read are not specified
anywhere in this document set.** SC-5 owns the composition point; nothing owns
the source.

### Open: column encryption has no trust root, and no fix covers both dialects

3.4 states the measured gap: `encrypted` is read from the creator-authored
descriptor at a single whole-stage gate, and deleting it stores plaintext at rest
on **both** Postgres and SQLite. Confirmed by two red probes through the real
write pipeline, not inferred.

What blocks it is that the obvious fix is dialect-split. Parameter typing works
only on Postgres, and fights the deliberate schema-blind coercion model there;
SQLite has no type fence at all because affinity is a preference. The remaining
option is a structural fence keyed on a fact the creator cannot author - the
physical column type - which means reading the live schema.

**That reader does not ship.** `backend::pg_introspect` is
`#[cfg(any(test, feature = "test-helpers"))]` (`backend/mod.rs:87`), and 3.1
removed live-catalog reads from the CRUD path deliberately (`descriptor.rs:12-28`).
So this is blocked on the same decision as the dormant mask-drift sweep: whether a
live-schema read returns to the data plane at all, and if so on what cadence. The
two must be decided together; nothing else in this document depends on that answer,
and two separate defences now do.

### Open: SC-1's executable form is larger than SC-1

The round-7 protocol artifact
(`docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`) requires a supervisor and
a durable `FenceJobRegistry`, so that terminal delivery survives process death.
The contract mentions neither, and the codebase contains neither. Until "must
terminal delivery survive process death?" is answered, SC-1's implementation
step is either *write a reducer* or *write a reducer plus a durable job system*.
That is a multiple, not a detail.

---

## 7. Implementation sequence

Dependency-ordered. Generated declarations, fixtures, reference docs and gates
**co-land with each contract change**; repository policy requires every
producer, consumer, fixture and reference doc in the same patch.

1. **Fix the live defects that are genuinely independent**, each with its own
   regression test. Independent today: **L4, and only L4.** L1, L2, L3 and L6
   are coupled to later steps by their own intended end states - L1/L2 end in
   deleting the policy writers with an artifact replacement, L3 in deleting
   `__zsSchemaReady`, and L6 is closed rather than fixed. Repairing them
   "independently" would mean inventing throwaway intermediate APIs, which the
   no-shim rule forbids.
2. **Landed.** Total decode, the command-tag check, and the savepoint
   frame-effect fate. See section 5.
3. **Landed** (no SHA recorded - see section 5). `OwnedPooledClient` and the
   SQLite actor's reservation/cancel/rollback primitives.

   **This step was not behaviour-neutral.** SC-2 deliberately changes two
   documented, creator-visible behaviours (3.13), and D10 changes the capacity
   model (3.12). Whether the `docs/reference/sqlite-divergences.md` entry it
   retires co-landed is **not verified**, and the three questions D10 leaves
   open (3.12) are **not recorded as answered anywhere**. Both are owed against
   a step already marked done.
4. **Write SC-3**: the normative IR, source ledger, and parity harness.
5. **5a (behaviour-neutral): `DbService` ownership landed** (SC-5, no SHA
   recorded). The artifact/init channel it was grouped with did **not** land and
   is part of the cutover below.

   **5b (the identity substrate): BLOCKED** on Fork C's home (section 6). It
   carries `app_incarnation`, the tombstone, the expected-incarnation CAS, the
   authority-domain reader (nothing in production reads it today), and the wire
   that carries the incarnation to the worker beside `deploy_hash`.

   It is listed **before** the cutover because the cutover's binding must carry
   this identity. Building the fence after the thing it fences is not a
   sequencing preference; it is a window. Under the reverse order the only ways
   to satisfy 5c would be a placeholder incarnation, a bare app id, or a lazy
   "adopt whatever exists at first use" - and each one reinstates the
   stale-handle and same-id recreation hole Fork C exists to close.

   **5c (the irreducible cutover):** the private module map and binding, the
   SC-4 dev mechanism, replacement of every declared-schema reader, and deletion
   of registration - co-landing manifest, SDK and docs. This includes **building
   the replacement mask-policy wire**: the carrier is decided (the artifact
   channel that already carries the descriptor), and what is still owed is the
   field and its schema, the authoring surface, build-time validation, the
   packer emission path, and the runtime read that turns bytes into a
   `MaskPolicy`. The string `mask` appears zero times in
   `crates/zeroship-bundle/src/manifest.rs` and zero times in
   `sdks/vite-plugin/src/zship.ts`, so there is no latent route in either.

   **5c also lands the operator ceiling as worker configuration**, since the
   binding constructed in this step is where `ceiling INTERSECT draft` is
   computed. The ceiling must therefore reach `DbService` at composition first,
   and its source is unspecified (section 6).

   **Deleting registration removes four distinct effects**, and the module's own
   header enumerates them (`register_model/mod.rs:1-35`):

   1. **`cache_schema`** - the declared JSON the SQL builders consume.
   2. **`mark_model_registered`** - a per-thread flag, written independently of
      the cache.
   3. **What that flag gated**, which is the security-relevant one. Absence of
      registration meant *unprotected*, and it was reachable:
      `db.collection(name)` mints a collection for **any non-empty string**,
      with no registration, descriptor or authority check at all
      (`crates/zeroship-plugin-db/src/v8_classes/db.rs:119-139`). The name is
      attacker-chosen and can name a table a migration already created and
      masked. What that reached is stated in the query builder's own
      documentation (`crates/zeroship-schema/src/query.rs:3000-3012`):
      `schema = None` yields `SELECT *`, which returns the plaintext parent
      column, because the masked-sibling substitution happens only in the `Some`
      arm - and the same absence turns the write pipeline's encrypt and mask
      transforms into no-ops.

      **Table-name secrecy is not a security boundary.** The replacement must
      make *addressability* the thing authority decides. This is the L24 shape
      and it is why `collection_schema` has no `Option` (3.1) - the read-path
      half has shipped; the `db.collection` half has not.
   4. **The SQLite `ATTACH`** (`register_model/mod.rs:122-129`, `:173-206`),
      which the module itself calls "in the wrong place, and that is a known
      item" - while direct SQLite paths still bypass the ordinary route that now
      attaches (`exec.rs:352-372`).

   All four replacements land in 5c or the step is not done.
6. **Move DDL out of the data plane (D11)**, in the order invariant 5 states:
   vector and spatial index creation into the migration path first, then the
   four index helpers, then `audit.rs` and its `Backend` methods, then the lazy
   `CREATE TABLE IF NOT EXISTS` sites once the migration service provisions the
   two audit tables.
7. **Fail closed on a missing, invalid or unparseable descriptor, at boot**, per
   invariant 4. What remains here is the boot check.
8. **Land the writer/recovery/deprovision protocol, audit provisioning, and the
   dev migration paths.** This survives as a **migration-service** concern - it
   is how a schema change is applied safely - and it is **blocked on Fork C**
   (section 6), because deprovision and recreation are the part whose state has
   lost its home. Restore is bound by the same protocol: it must re-provision
   the per-app role and both `ALTER DEFAULT PRIVILEGES ... IN SCHEMA` entries
   (`crates/zeroship-migrate-server/src/apply.rs:1748-1751`), without which every
   table a *future* migration creates carries no grant; re-establish schema
   ownership, which `pg_restore --no-owner` leaves on the restoring login role;
   re-provision the per-app platform tables the `CASCADE` destroyed; and re-run
   publication reconciliation, without which subscriptions silently stop
   forever. The current terminal arm - release the lock with the schema empty
   (`restore` at `crates/zeroship-data-postgres/src/postgres.rs:871`,
   delegating to `backup_pg::restore_impl`) - is not
   acceptable. A restore that silently replaces the unmask audit trail with an
   older one is an **audit-erasure primitive**, and the rewind is recorded as an
   operator-visible event.

   **The signature change is the point.** `apply_ir_documents` currently takes a
   **DSN** and opens its own session inside
   (`crates/zeroship-migrate-server/src/apply.rs:258-259`, connect at `:460`),
   so a caller has nothing to scope a transition to. It takes an
   already-connected session instead, and the caller owns the whole transition.
   The engine already takes its own project advisory lock (`apply.rs:1053-1057`,
   acquired at `:1080-1087`), and app roles `INHERIT IN ROLE` the app-role
   template (`apply.rs:1732`).
9. **Land per-subscription projection.** It is a masking and descriptor concern
   and is unaffected by D6/D7. The schema-change signal a subscriber needs
   across a migration or a restore is the CDC service's in-WAL marker, not this
   step's.
10. **Land the owned transaction registry and state machine (SC-1)** and
    randomized atomicity.
11. **Port plan families and non-query capabilities.** Requires step 4 to have
    produced the normative types, not the family sketch: SC-3 says in its own
    words that it is not a finished grammar, and it inventories only `query.rs`
    while this document hands it the non-query capability signatures too.
12. **Delete `BackendHandle` and the thread-local caches, and retire
    `zeroship-schema`**, when SC-3's ledger reaches zero **and** the five-module
    audit in 3.8 is done. The `AGENTS.md` correction lands here at the latest.

---

## 8. Acceptance criteria

Stated as failing-test shapes. **An arm is evidence only if it was built, ran,
and ruled on something** - not built, filtered out, skipped, aborted partway,
and never scheduled are five different ways of printing something other than a
red. The five mechanisms by which this codebase's tests have reported green
while ruling on nothing are in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`, and every
arm below is subject to them.

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
registers a model, mutates declared schema, or writes policy, and no policy
value originating in an isolate is persisted.

**Fail-closed access.** A missing, invalid or unparseable descriptor fails boot
and issues no data SQL, asserted with a counting executor - **zero** metadata
round trips, not "none after a failure". Missing metadata is observably distinct
from `VerifiedPlain`. A plaintext-to-masked redeploy cannot reuse a stale
projection. A filter key whose getter throws fails the operation and never
produces a predicate that is a strict subset of the declared filter.

**One arm that should exist and cannot.** A descriptor that is well-formed but
does not match the database is not detected at runtime. That is the
deploy-ordering failure in 3.1, and its arm belongs to the deploy precondition
and the flip, not here; writing one against today's runtime would be writing an
arm nothing can pass.

**Delivery.** A mask-only field's plaintext never appears in a CDC event, WS
frame, or live-query payload; the test creates the column through a real
migration and reads a real WAL event - a hand-built `ChangeEvent` fixture does
not satisfy it.

**Policy.** A binding constructed under a lowered ceiling denies `unmask` **by
an actor the effective ceiling governs** - **and an unmask that same ceiling
still permits succeeds in the same test**, since a deny-only arm passes on an
implementation where the meet is broken and everything is denied. A ceiling that
revokes `auto` denies `auto` even when the creator draft does not mention `auto`
at all: the naive map intersection **inverts** that revocation, and it passes
every other arm. No V8 method writes a policy, and no policy value originating
in an isolate is persisted or read.

**Cost.** A warm autocommit operation issues at most **three** server round
trips, pinned **after** the step that ports the plan families rather than
before. **That bound is loose, and the arm is non-discriminating in the
direction that matters:** with no epoch read and no per-operation lease in
`prepare`, a warm operation does strictly less than three, so the arm would pass
on an implementation that reintroduced a metadata round trip. **The arm must be
re-derived from what `prepare` still does - session setup - and that measurement
has not been made.**

The pool has **two** validation sources, not one, and both are excluded from the
count and asserted separately: a dirty checkout runs a validation `simple_query`
before use, and a *clean* connection idle beyond 500 ms pays an alive-validation
round trip (`pool.rs:1198-1208`).

**Backend and boundary.** Equivalent resolved metadata across backends for one
logical fixture. No SQLite I/O during construction or binding. A missing app
file returns `SCHEMA_NOT_APPLIED` and creates no file. SQLite URL vectors
include `?`, `#`, `%`. Dropping a caller-side future races the actor command's
completion: cancellation winning interrupts, rolls back and retires before
acknowledging; completion winning yields `AlreadyCompleted` and claims no
rollback (SC-2). Not "cancels and rolls back" unconditionally, which cannot pass
- the actor may commit and reply before the caller polls.

**Module boundary.** `backend/api.rs` exists (it does not today) and no driver
types appear above it; one file names both backends. `zeroship-plugin-db` has no
`zeroship-schema` dependency and SC-3's ledger has no unported entries. **No
data-plane path executes DDL**, enforced by deleting the sites enumerated in
invariant 5 rather than only by a classifier.

**Gates.** Every arm declares the number of items it ruled on and a floor that
number must clear, per `tests/lib/gate_arms.sh`. Two specific traps this design
has already hit:

- **A source gate scoped to a path that does not exist matches nothing and
  reports success.** Every gate in 3.8 asserts its file exists first.
- **A gate that greps for a literal cannot see an interpolated site.** A gate
  for `CREATE SCHEMA "__zeroship_admin"` matched zero lines in `crates/` and
  `libs/` while the schema was nonetheless created, because the real site
  interpolated the identifier. Any such arm must match the **interpolated**
  form and be **proved against a fixture containing the `format!` spelling**
  before it is trusted.

---

## 9. Risks

| Risk | Mitigation |
| --- | --- |
| A deploy goes live before its migration applies | The pipeline's ordering guarantee is an invariant (3.1); D12 makes it an enforced precondition, with the flip behind it. Nothing at runtime detects a violation |
| A migration is applied while workers run | Roll the workers. Stated as a procedure because there is no mechanism (3.1) |
| Resolution rules drift between backends | Resolution lives in `frontend/metadata.rs`; backends emit neutral facts and share parity fixtures |
| Pinned code incompatible with new schema | Bounded by `max_pinned_isolates_per_app`; force-eviction is the lever |
| The owned-checkout extension is larger than expected | It is a named step with its own parity tests, landed before anything depends on it |
| SC-1..SC-6 are written thinly and the same gap recurs | Each has a stated acceptance shape; the work they gate does not start until they are reviewed |
| Gate counts fall as registration code is deleted | Replace arms with end-state behavioural and absence checks, each declaring its ruled-on count and floor |
| The suite the arms run in races, so no arm's verdict is trustworthy | Fixed for `zeroship-plugin-db` in `fc2c889db`, and worth keeping stated as a standing risk because it silently degrades every criterion in these documents |

---

## 10. Rejected alternatives

**Rename `registerModel`** - preserves the wrong lifecycle. **Separate crates
per backend** - the problem is mixed ownership, not package count. **Keep
`BackendHandle` matches but move their bodies** - leaves every caller aware of
both dialects. **Keep an asynchronous schema-ready promise** - nothing
asynchronous remains to represent. **Mutate schema metadata during dev HMR** - a
fresh isolate is a testable boundary. **Store mask policy in the deploy
descriptor only** - pins authorization to old code, which is why the ceiling is
a separate half.

**A database-resident schema epoch, with a shared lease and per-operation live
introspection.** Rejected because its whole job was to make a second authority
trustworthy and cheap, and there is no second authority. The evidence that
settled it, and the two measured results worth keeping from it, are in the
decision log.

**A userspace cross-process flock lease for SQLite** - rebuilds what WAL
snapshot isolation already provides, and brings its own starvation and
per-open-file-description problems.

**Process-wide singleflight** - needs a cross-thread wake path unverified here.

---

## 11. Documentation updates

`docs/reference/db.md`, `docs/reference/plugin-system.md`,
`docs/architecture/runtime.md`, `docs/reference/vite-plugin.md`,
`docs/reference/sqlite-divergences.md`, `sdks/bootstrap/README.md`, relevant
crate READMEs, and **`AGENTS.md`'s stale claim that the migration engine reuses
`zeroship-schema`** (3.8). Each co-lands with the change it describes.

The documentation should say: migrations define and apply schema ahead of
runtime; the folded descriptor installs bindings and is the sole authority for
schema; the mask policy is **declared in the creator's codebase and delivered in
the deploy artifact**, met with an operator ceiling that is worker
configuration, and is never an isolate input; and runtime DB access has no
model-registration phase.

**One creator-visible change owes `docs/reference/db.md` an entry now**, beside
the mask kinds: `defineMaskPolicy` is gone and policy is declared in the
codebase instead. A second lands with the storage flip: equality search by real
value (`find({ssn: "123-45-6789"})`) stops matching until the keyed lookup
column ships. A creator should learn either one when they declare the mask, not
when a call stops working.

---

## 12. Final state

| Former effect | New owner |
| --- | --- |
| Build collection wrappers | Synchronous private pre-user binding |
| Hold declared logical metadata | Isolate-owned immutable `DbIsolateBinding` |
| Verify physical/security metadata | **Nobody, at runtime.** The descriptor asserts it; the deploy pipeline's enforced ordering is what makes the assertion true; the storage flip is the second line of defence |
| Select configured backend | The single `backend/factory.rs` composition point |
| Lower and execute a plan | The selected concrete backend |
| Attach SQLite app database | SQLite session preparation |
| Apply schema, and emit any DDL | Migration service / explicit dev migration path |
| Gate request readiness | Nothing; construction completes before creator evaluation |
| Apply mask policy | Worker configuration (operator ceiling) meet deploy artifact (creator draft), resolved once at binding construction |
| Custody of column encryption keys | Derived from the platform master key at the service; no key table, no getter |
| Record a PITR target | Control plane |
| Establish per-request identity | The Rust call boundary in the worker; no SQL-side session |
| Own replication slots and publications | The CDC service |
| Fence a stale handle across deprovision | **Unhomed.** Fork C is specified; its storage is open (section 6) |

Schema is applied before runtime, bindings are constructed with the runtime, and
every data operation reads the descriptor its deploy was built with.

---

## 13. The rest of the document set

| Document | What it is |
| --- | --- |
| `2026-08-26-runtime-db-binding-00-index.md` | The set's landing page: what each document is for and what is undecided |
| `2026-08-26-runtime-db-binding-decision-log.md` | The superseded record: every decision and correction, newest first, with the evidence that settled it |
| `2026-08-26-runtime-db-binding-defect-register.md` | Defects in existing code that this design touches, with their status. The most perishable file in the set |
| `2026-08-26-runtime-db-binding-defects-closed.md` | The closed defects, each with its closing commit and evidence |
| `2026-08-26-runtime-db-binding-verification-record.md` | How this codebase's tests report green while ruling on nothing. The most durable |
| `2026-08-26-sc1-transaction-protocol.md` | Transaction state machine, frames and effects, guard order, property invariants |
| `2026-08-26-sc2-sqlite-actor-protocol.md` | SQLite actor: reservations, the four cancellation interleavings, the terminal classifier |
| `2026-08-26-sc3-dbplan-ir-and-ledger.md` | The `DbPlan` IR, its source ledger, the parity harness |
| `2026-08-26-sc4-dev-and-hmr-mechanism.md` | Dev tier and hot reload. Thinnest |
| `2026-08-26-sc5-service-ownership.md` | `DbService`, process-wide cache, per-thread driver resources, the durable app incarnation. **Fork C is specified here, not in SC-6** |
| `2026-08-26-sc6-ceiling-read-contract.md` | The ceiling meet, joined reads, and the masking storage flip |
| `2026-08-28-cdc-service.md` | The CDC service that owns WAL consumption, the wire projection, and the schema-change signal |
| `2026-08-28-deploy-schema-precondition.md` | Refusing to make a deploy live until its migrations have applied |

Reviews this design depends on:

| Review | What it settles |
| --- | --- |
| `docs/reviews/2026-08-27-descriptor-specification.md` | Every data-plane consumer of a schema fact, classified. The bucket "genuinely requires the live database" came back empty of schema facts, which is what made D7 implementable rather than hoped-for |
| `docs/reviews/2026-08-27-query-by-plaintext.md` | A keyed blind-index column plus a `findByUnmasked` verb: the flip's owed lookup item |
| `docs/reviews/2026-08-28-flip-write-path.md` | The flip's write path and what it must fix before it can be implemented |
| `docs/reviews/2026-08-28-sqlite-authority-row.md` | The SQLite authority row's fate |
| `docs/reviews/2026-08-27-migrate-crate-survey.md` | The migrate crates' boundaries |
| `docs/reviews/dbbind-2026-08-26/` | Seven review rounds plus a performance round, three independent reviewers apiece |
