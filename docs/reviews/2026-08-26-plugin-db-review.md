# Plugin DB access-pipeline review

**Status:** Active - findings are open unless marked otherwise

**Review date:** 2026-08-26

**Scope:** `crates/zeroship-data-v8`, the runtime/bootstrap integration,
`@zeroship/db`, and the shared SQL builders in `zeroship-schema`

**Branch inspected:** `main` at `f72a4b30b`

This is a read-only review report. It records defects and recommended end
states; it does not imply that fixes were included with the report.

## Executive summary

The ordinary SQL execution path has strong foundations:

- `app_id` and collection identity are stamped in native V8 wrappers rather
  than accepted from creator arguments;
- values are parameterized and identifiers are validated and quoted;
- PostgreSQL autocommit operations apply the per-app role and timeouts with
  `SET LOCAL` inside an explicit transaction;
- transaction routing is captured synchronously at the V8 boundary;
- write and read transforms are centralized; and
- metering is emitted by the primitive only after successful operations.

The main production risks sit at lifecycle boundaries rather than in basic SQL
rendering:

1. creator modules run before the bootstrap has consumed its privileged
   globals;
2. metadata described as per-isolate is actually shared by every isolate on a
   worker thread;
3. transaction settlement cannot distinguish an absent connection from a
   temporarily checked-out one;
4. randomized-encryption operations span multiple non-atomic statements; and
5. the V8-to-JSON boundary has no breadth or total-size budget.

`registerModel` is no longer a migration or DDL operation. On PostgreSQL its
database arm is a no-op; on SQLite it redundantly attaches the app file now
that ordinary execution also ensures attachment. Its remaining load-bearing
effects are the registration marker and the declared-schema cache, both of
which currently participate in security-sensitive behavior.

## DB access pipeline

### 1. Schema authority and deploy

- Committed migration operations are folded into the runtime schema
  descriptor.
- `zeroship-migrated` applies PostgreSQL schema changes before the deploy goes
  live.
- The explicit `zeroship-dev-migrate`/`pnpm migrate` path applies SQLite
  migrations. Vite dev regenerates and reports descriptor/schema state but
  deliberately does not apply migrations when it starts or reloads.
- The `.zship` carries the runtime descriptor that the worker supplies to the
  runtime.

Relevant code:

- `crates/zeroship-worker/src/cache.rs:366`
- `crates/zeroship-worker/src/cache.rs:485`
- `crates/zeroship-runtime/src/core/init.rs:3407`

### 2. Runtime and schema bootstrap

- The DB plugin mints an `env.db` V8 object bound to the runtime-provided
  `app_id`.
- `mint_db` records the deploy token in plugin DB context.
- The synthetic runtime entry reads the runtime descriptor, installs typed SDK
  `Collection` wrappers synchronously, and starts the per-collection
  `registerModel` promise chain.
- Request dispatch waits for `__zsSchemaReady`.

Relevant code:

- `crates/zeroship-data-v8/src/v8_classes/db.rs:294`
- `sdks/bootstrap/src/runtime-entry.ts:73`
- `sdks/bootstrap/src/install-schema.ts:1152`
- `sdks/bootstrap/src/install-schema.ts:1286`
- `sdks/bootstrap/src/install-schema.ts:1307`

### 3. SDK call boundary

The SDK waits for collection readiness, validates and maps documents and
filters, applies naming strategy and soft-delete conventions, resolves the
native collection, and converts native exceptions to the documented
`Result<T>` surface outside transactions.

Relevant code:

- `sdks/db/src/collection.ts:252`
- `sdks/db/src/collection.ts:268`
- `sdks/db/src/collection.ts:295`
- `sdks/db/src/collection/crud.ts:196`

### 4. Native V8 boundary

The native `Collection` carries immutable `app_id` and collection name. Each
method decodes V8 arguments, applies the query-versus-mutation capability
fence where appropriate, captures `TxRoute`, and queues the asynchronous
operation on the runtime pump.

Relevant code:

- `crates/zeroship-data-v8/src/v8_classes/collection.rs:29`
- `crates/zeroship-data-v8/src/v8_classes/collection.rs:63`
- `crates/zeroship-data-v8/src/crud/mod.rs:606`
- `crates/zeroship-data-v8/src/crud/mod.rs:772`

### 5. Transform and query construction

Writes run the following ordered stages:

1. user-field validation;
2. system-field injection or update hints;
3. any row-ID resolution required by randomized encryption;
4. encryption;
5. masked-sibling generation; and
6. dialect-specific binary lowering.

Reads decode backend rows, normalize typed values, decrypt eligible fields,
wrap masked values, and apply explicitly authorized unmask overrides.

The query builders validate and quote identifiers and keep values in bound
parameters.

Relevant code:

- `crates/zeroship-data-v8/src/crud/write_pipeline.rs:72`
- `crates/zeroship-data-v8/src/crud/read_pipeline.rs:34`
- `crates/zeroship-schema/src/query.rs`

### 6. Backend execution and return path

- PostgreSQL operations use either the structurally captured transaction
  connection or a short explicit transaction carrying `SET LOCAL ROLE` and
  timeout guards.
- SQLite operations use the shared session actor and attached per-app file.
- Successful mutations produce local or deferred broker events; successful
  operations produce infrastructure-owned metrics.
- Rows are decoded to `serde_json::Value`, transformed into V8 values, and
  returned through the SDK error/result mapping.

Relevant code:

- `crates/zeroship-data-v8/src/exec.rs:174`
- `crates/zeroship-data-v8/src/exec.rs:210`
- `crates/zeroship-data-v8/src/exec.rs:293`
- `crates/zeroship-data-v8/src/exec.rs:409`
- `crates/zeroship-data-v8/src/v8_bridge.rs:315`

## Findings

### DBR-01 - Critical - creator code runs before privileged bootstrap state is consumed

**Status:** Open

The runtime installs `globalThis.__zsDbPlatform` and
`globalThis.__zsRuntimeDescriptor` before evaluating the synthetic bootstrap.
The bootstrap statically imports `./__user__.js`, so ESM dependency evaluation
runs creator top-level code before the bootstrap body reads the descriptor or
deletes the resolver.

Creator code can therefore:

- retain the app-scoped `DbPlatform` handle indefinitely;
- invoke `registerModel`, `setMaskPolicy`, or replication operations later;
- mutate the validated descriptor before `installSchema` consumes it; or
- delete the descriptor and make bootstrap degrade to a schema-less app.

This is demonstrated indirectly by the production-style transaction test,
which captures the platform handle at creator-module top level. The dedicated
platform-fence test checks only a later handler, after cleanup, and therefore
does not cover the vulnerable boundary.

The security impact is larger than exposing an internal method. When the
descriptor is removed, `registerModel` never marks the collection ready and
`runtime_schema_for` returns `None`. Writes then skip encryption and mask
generation, while raw reads can fall back to an unqualified `SELECT *` shape.
A forged descriptor can likewise steer declared-schema consumers such as
masked `distinct` projection.

Evidence:

- `crates/zeroship-runtime/src/core/init.rs:1642`
- `crates/zeroship-runtime/src/core/init.rs:3395`
- `crates/zeroship-runtime/src/core/init.rs:3407`
- `sdks/bootstrap/src/runtime-entry.ts:73`
- `sdks/bootstrap/src/runtime-entry.ts:215`
- `crates/zeroship-data-v8/tests/native_transaction.rs:368`
- `crates/zeroship-data-v8/tests/platform_fence.rs:108`
- `crates/zeroship-data-v8/src/crud/introspect_schema.rs:68`

Recommended end state:

- consume the descriptor and platform capability in a pre-user internal
  module or native bootstrap step;
- keep both values in native/private or lexical state rather than writable
  globals; and
- add a regression test that attempts module-scope capture, descriptor
  mutation, descriptor deletion, and later handle reuse.

### DBR-02 - Critical - registration metadata is not isolate or deploy scoped

**Status:** Open

`IsolateDbContext` is stored in one `thread_local!`, so it is actually shared
by every isolate on a worker OS thread. Normal isolates and multiple
deploy-pinned workflow isolates for the same app can coexist on that thread.

The following keys omit deploy or isolate identity:

- `registered_models`: `app_id:collection`;
- declared `schemas`: `app_id:collection`; and
- deploy token: one mutable value per `app_id`.

`mint_db` overwrites that single deploy token only when an isolate is created.
It is not restored when an existing isolate is re-entered. A redeploy can also
hit `registerModel`'s warm fast path and return before caching its descriptor.

Consequences include:

- the previous deploy's declared schema surviving a redeploy;
- current and pinned workflow deploys overwriting one another's token and
  metadata;
- stale typed-ID prefixes;
- stale encryption mode, key, or wrapping hints on SQLite;
- stale unmask classifications; and
- unsafe masked SQL projection.

Concrete disclosure case: deploy A declares an existing field as plaintext;
deploy B changes it to mask-only. If B inherits A's declared cache,
`distinct()` selects the plaintext parent column, records that it did not read
the masked sibling, disables mask wrapping, and flattens the plaintext value to
JavaScript.

Evidence:

- `crates/zeroship-data-v8/src/context.rs:112`
- `crates/zeroship-data-v8/src/context.rs:134`
- `crates/zeroship-data-v8/src/context.rs:233`
- `crates/zeroship-data-v8/src/context.rs:542`
- `crates/zeroship-data-v8/src/context.rs:648`
- `crates/zeroship-data-v8/src/context.rs:888`
- `crates/zeroship-data-v8/src/v8_classes/db.rs:313`
- `crates/zeroship-worker/src/cache.rs:21`
- `crates/zeroship-worker/src/cache.rs:485`
- `crates/zeroship-data-v8/src/register_model/mod.rs:79`
- `crates/zeroship-data-v8/src/crud/mod.rs:1846`

Recommended end state:

- store runtime metadata in the actual runtime/isolate instance; or
- key every registration, declared schema, introspection cache, mask policy,
  and deploy token by captured `(app_id, deploy_hash or isolate_id)`.

Clearing app-level state when the token changes is insufficient because old
and current workflow deploys legitimately coexist.

### DBR-03 - High - transaction settlement can skip terminal SQL

**Status:** Open

A transaction CRUD operation temporarily removes the only transaction client
from context while awaiting database I/O. The top-level settlement future can
run concurrently and interprets the resulting empty slot as an already-drained
transaction. It releases the claim, clears pending events, and returns success
without sending `COMMIT` or `ROLLBACK`.

Example:

```ts
db.transaction(async (tx) => {
  tx.users.insert({ name: "not awaited" });
});
```

If the insert has checked out the connection before the callback resolves,
settlement can report success and the insert can later restore an open
transaction connection. A rejecting `Promise.all` creates the same rollback
hole while another operation remains in flight.

This violates the documented promise that work escaping the callback is
refused with `TRANSACTION_SCOPE_EXPIRED` rather than silently surviving
settlement.

Evidence:

- `crates/zeroship-data-v8/src/exec.rs:174`
- `crates/zeroship-data-v8/src/exec.rs:352`
- `crates/zeroship-data-v8/src/transaction/mod.rs:824`
- `crates/zeroship-data-v8/src/transaction/mod.rs:974`
- `crates/zeroship-runtime/src/core/runtime.rs:177`
- `crates/zeroship-runtime/src/core/runtime.rs:2645`
- `docs/reference/db.md:975`

Recommended end state:

- represent the slot as an explicit state machine such as parked, checked out,
  settling, and settled;
- make settlement wait for a checked-out lease; and
- never interpret an absent client as proof that terminal SQL already ran.

Add regressions for an unawaited operation and for a rejecting `Promise.all`
while another operation is pending.

### DBR-04 - High - randomized-encryption upsert can persist undecryptable ciphertext

**Status:** Open

The randomized-encryption upsert path:

1. creates an insert ID;
2. probes the conflict target for an existing row ID;
3. encrypts using the selected ID as AAD; and
4. later executes `INSERT ... ON CONFLICT DO UPDATE`.

If the probe sees no row and another writer inserts the conflict winner before
step 4, this operation encrypts for minted ID `X` but the conflict branch keeps
the existing row ID `Y`. The encrypted field is updated onto row `Y` while its
ciphertext remains bound to `X`. Reads reconstruct AAD from `Y`, so AEAD
verification fails permanently.

Evidence:

- `crates/zeroship-data-v8/src/crud/write_pipeline.rs:151`
- `crates/zeroship-data-v8/src/crud/write_pipeline.rs:456`
- `crates/zeroship-data-v8/src/crud/mod.rs:1965`
- `crates/zeroship-schema/src/query.rs:5889`
- `crates/zeroship-schema/src/query.rs:5919`
- `crates/zeroship-data-v8/src/crud/encryption_pass.rs:335`

Recommended end state:

Atomically establish and lock the conflict winner's stored ID before
encryption, or detect a different returned winner ID and retry encryption and
the update inside one transaction. A preflight `SELECT` at read-committed
isolation is not sufficient.

### DBR-05 - High - randomized encrypted updates lose ordinary update semantics

**Status:** Open

Randomized encryption needs each target row's ID for AAD. The implementation
first selects target IDs, then issues updates by ID.

Two correctness problems follow:

1. `updateOne` and `updateMany` discard the original predicate for the final
   update. A concurrent transaction can change a filter field after the
   preselection, but the stale row ID is still updated even though the row no
   longer matches the caller's filter.
2. Outside a creator transaction, randomized `updateMany` executes one
   autocommit transaction per row. If a later row fails, earlier rows remain
   committed even though the overall operation rejects. The non-randomized
   path is a single SQL statement, so behavior changes based on encryption
   mode.

Evidence:

- `crates/zeroship-data-v8/src/crud/write_pipeline.rs:318`
- `crates/zeroship-data-v8/src/crud/mod.rs:1002`
- `crates/zeroship-data-v8/src/crud/mod.rs:1077`
- `crates/zeroship-data-v8/src/crud/mod.rs:1251`
- `crates/zeroship-data-v8/src/crud/mod.rs:1305`
- `crates/zeroship-data-v8/src/exec.rs:284`

Recommended end state:

- preserve `original predicate AND id` in the final mutation;
- execute the multi-row algorithm in one internal transaction; and
- prefer an atomic CTE or backend operation that selects, transforms, and
  updates a stable target set.

### DBR-06 - High - sparse V8 arrays can exhaust shared-worker native memory

**Status:** Open

The V8-to-`serde_json` decoder caps recursion depth but not breadth, total node
count, or decoded bytes. Its array arm trusts `Array.length`, immediately calls
`Vec::with_capacity(length)`, and visits every index.

`new Array(4294967295)` is cheap and sparse in V8 but asks Rust to reserve
billions of `Value` slots and then loop over them. Because decoding happens
before query-builder validation, the later 1,000-document `insertMany` limit
cannot protect the worker. One creator isolate can therefore exhaust or stall
a shared worker thread outside the V8 heap limit.

The same decoder silently converts excessive depth, non-finite numbers,
functions, and symbols to JSON `null`. For filters, `Infinity` becoming `null`
can change the operation to `IS NULL`; for writes it can silently store null.

Evidence:

- `crates/zeroship-data-v8/src/v8_bridge.rs:140`
- `crates/zeroship-data-v8/src/v8_bridge.rs:162`
- `crates/zeroship-data-v8/src/v8_bridge.rs:174`
- `crates/zeroship-data-v8/src/v8_bridge.rs:221`
- `crates/zeroship-data-v8/src/v8_classes/collection.rs:90`
- `crates/zeroship-schema/src/query.rs:601`
- `crates/zeroship-schema/src/query.rs:4035`

Recommended end state:

- make argument decoding fallible;
- enforce per-array, per-object, total-node, and decoded-byte budgets before
  allocation;
- reject excessive nesting and unsupported/non-finite values with a stable
  validation code; and
- regression-test sparse arrays without actually allocating near the limit.

### DBR-07 - Medium - `installSchema` does not serialize registration execution

**Status:** Open

`_installInFlight` protects only the synchronous JavaScript call frame and is
reset before registration promises settle. Every invocation starts its own
eager `Promise.resolve()` chain. `_prevChain` is consulted only afterward when
constructing the returned `ready` promise.

Promises are eager, so this delays observation of a second chain but does not
delay its execution. Two installs can call native `registerModel` concurrently.
Both can pass the native warm-cache check before either task marks the model;
completion order then decides which schema is cached.

Evidence:

- `sdks/bootstrap/src/install-schema.ts:927`
- `sdks/bootstrap/src/install-schema.ts:1145`
- `sdks/bootstrap/src/install-schema.ts:1308`
- `sdks/bootstrap/src/install-schema.ts:1530`
- `crates/zeroship-data-v8/src/register_model/mod.rs:79`

Recommended end state:

Seed the registration work itself from the previous chain, rather than only
wrapping the returned observer. Native metadata installation should also be a
deploy-scoped keyed single-flight or reject conflicting descriptor hashes.

If `registerModel` is removed as recommended below, parse the descriptor into
immutable isolate-owned state during construction; do not replace it with a
callable installer.

### DBR-08 - Medium - `registerModel` violates its `Promise<void>` contract

**Status:** Open

The warm path resolves JavaScript `undefined`. The cold success path emits
`ResolveValue::String("null")`, which materializes the actual JavaScript string
`"null"`, not JSON null or undefined.

Evidence:

- `crates/zeroship-data-v8/src/register_model/mod.rs:79`
- `crates/zeroship-data-v8/src/register_model/mod.rs:107`
- `crates/zeroship-runtime/src/core/state.rs:939`
- `crates/zeroship-runtime/src/core/state.rs:947`

Recommended end state:

Resolve both paths with `ResolveValue::Undefined` and add a test that compares
cold and warm results.

### DBR-09 - Medium - the retained ignored wire arguments violate repository policy

**Status:** Open

The current V8 method retains `indexes` and `declared` as accepted-but-ignored
arguments to preserve older positional callers. The test-only dispatch seam
similarly retains unused collection/schema/index arguments for resemblance to
the old call.

Zeroship has no production creator apps and explicitly forbids compatibility
shims for SDK, V8 RPC, and wire contracts. Keeping dead arguments for older raw
deploys contradicts that rule and makes the API continue to imply work that no
longer exists.

Evidence:

- `crates/zeroship-data-v8/src/v8_classes/db_platform.rs:100`
- `crates/zeroship-data-v8/src/v8_classes/db_platform.rs:108`
- `crates/zeroship-data-v8/src/register_model/mod.rs:220`
- `AGENTS.md:13`
- `AGENTS.md:23`

Recommended end state:

Update `installSchema`, internal declarations, tests, and the V8 method in the
same patch, then delete the ignored arguments. Do not keep an old arity or
detect-and-warn path.

### DBR-10 - Medium - SQLite attachment remains duplicated in metadata registration

**Status:** Partially mitigated; cleanup remains open

SQLite execution now calls `attach_app_file(route.app_id())` before routing an
operation, so raw/schema-less access no longer depends solely on registration.
`registerModel` still performs the same attach, however, and its module-level
documentation still describes itself as the only production caller. The
duplicated responsibility obscures the real lifecycle boundary and leaves the
current `ATTACH` path able to create an empty file when the migration step was
never run.

Evidence:

- `crates/zeroship-data-v8/src/register_model/mod.rs:22`
- `crates/zeroship-data-v8/src/register_model/mod.rs:129`
- `crates/zeroship-data-v8/src/exec.rs:360`
- `crates/zeroship-data-v8/src/backend/sqlite/mod.rs:669`
- `crates/zeroship-data-v8/src/backend/sqlite/session.rs:1070`

Recommended end state:

Make backend session preparation the single attachment owner. It must acquire
the app's shared schema lease, attach the existing app file with `mode=rw`, and
carry the guard through execution. A missing migrated file must return
`SCHEMA_NOT_APPLIED` without creating one. Remove attachment from schema
metadata installation.

### DBR-11 - Medium - transaction claims can survive cancellation and isolate eviction

**Status:** Open; the pre-BEGIN window is already documented in source

After acquiring the top-level transaction claim, cancellation before `BEGIN`
returns and installs the client leaks the claim. The waiting future has no
drop guard. Because the state is worker-thread-local rather than runtime-owned,
evicting the originating isolate does not clear it, so later transactions for
that app can remain parked indefinitely.

A callback promise that never settles also retains its raw transaction
finalizer for the life of the isolate.

Evidence:

- `crates/zeroship-data-v8/src/transaction/mod.rs:305`
- `crates/zeroship-data-v8/src/transaction/mod.rs:324`
- `crates/zeroship-data-v8/src/transaction/mod.rs:399`
- `crates/zeroship-data-v8/src/transaction/mod.rs:762`
- `crates/zeroship-data-v8/src/context.rs:888`

Recommended end state:

- arm an RAII claim guard as soon as the claim is acquired;
- disarm it only after the transaction client is installed;
- make transaction finalizer ownership non-leaking; and
- add explicit isolate teardown for any state that remains shared.

## `registerModel` assessment

The current name and asynchronous shape preserve the mental model of runtime
DDL, but the actual behavior is:

| Concern | PostgreSQL | SQLite |
| --- | --- | --- |
| Apply schema | No | No |
| Can trigger lazy backend initialization | Yes | Yes |
| Attach app storage | No | Yes, redundantly with execution |
| Mark collection registered | Yes, in dispatch success arm | Yes, in dispatch success arm |
| Cache declared schema | Yes, in dispatch success arm | Yes, in dispatch success arm |

The marker is a security-relevant gate: `runtime_schema_for` refuses to
introspect before it is set. The declared cache is also not merely a typing
hint. It feeds masked SQL projection, unmask metadata, typed-ID prefixes, and
the SQLite metadata fallback.

The clean pre-launch end state is:

1. parse and bind the runtime descriptor automatically while constructing each
   isolate, with no successor registration method;
2. complete binding before creator module evaluation without exposing a
   resolver or mutable global;
3. make SQLite session preparation the sole no-create attachment owner;
4. remove the registration prerequisite from PostgreSQL live introspection or
   otherwise make missing security metadata fail closed;
5. source security-sensitive SQL projection and post-read transforms from the
   same authoritative schema snapshot; and
6. remove the dead index/declaration arguments, topological registration work,
   and remaining DDL/advisory-lock language from bootstrap docs and tests; and
7. move the neutral descriptor, planning, and sentinel logic that remains
   necessary into `zeroship-data-v8`, without depending on
   `zeroship-schema` or adding a new contract crate.

The full replacement design is in
`docs/proposals/2026-08-26-runtime-db-binding-design.md`.

## Verification performed during review

The following checks passed against the inspected branch state:

```text
cargo check -p zeroship-data-v8 --all-targets
cargo test -p zeroship-data-v8 --test platform_fence
```

The platform-fence test reported two passing tests. That result does not close
DBR-01 because both assertions run after module evaluation; neither attempts a
creator module-scope capture or descriptor mutation.

No live PostgreSQL or multi-isolate workflow integration suite was run as part
of this review. The transaction and encryption-race findings require new
deterministic concurrency regressions.
