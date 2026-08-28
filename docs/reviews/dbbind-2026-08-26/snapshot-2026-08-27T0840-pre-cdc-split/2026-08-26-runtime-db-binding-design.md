# Proposal: replace `registerModel` with construction-time DB binding

**Date:** 2026-08-26 (v4, revised through seven review rounds plus a dedicated
performance round; the artifacts are in `docs/reviews/dbbind-2026-08-26/`)

**Status:** PROPOSED - **direction settled and gated**. The first three rounds
of three independent reviewers returned: implement / implement / conditional
pass. Independent work may start now; the six sub-contracts below gate the work
that depends on them, and the numbered sequence should not be followed literally
until each is written at its declared gate.

**Scope:** `zeroship-plugin-db`, runtime and worker initialization,
`@zeroship/bootstrap`, `@zeroship/db`, the migration/runtime schema lock, restore,
app lifecycle, CDC delivery, and Vite development boot

This is a breaking pre-launch design. The cutover deletes `registerModel`; it
does not retain an alias, compatibility mode, or dual registration path.

---

## Status of this document

**Seven review rounds have run, plus a dedicated performance round.** The
artifacts for the latest of each are in `docs/reviews/dbbind-2026-08-26/`
(`dbbind-r7-*`, `perf-r1-*`), three independent reviewers apiece. This section
still recounts the **first two**, because they are what the corrections table
below answers: each had three independent reviewers (security/protocol,
performance/architecture, implementability); round 1 found v1 not implementable,
and round 2 reviewed the revision and found ten errors introduced *by the
revision*, plus eight sentences that could not be turned into an unambiguous
failing test.

v3 does three things:

1. **Corrects every error found in v2.** They are listed below with evidence,
   including the ones that were mine.
2. **Stops asserting artifacts this document does not contain.** Where v2 said a
   contract "is specified in full", v3 names it as a **required sub-contract**
   with an owner and an acceptance shape. Writing more prose asserting them
   would repeat exactly the failure round 2 caught.
3. **Adopts a dependency-correct sequence** in place of v2's seven merges, which
   were inverted in seven concrete ways.

Measurements cited as MEASURED were taken by the author against PostgreSQL
16.14 in a dedicated container, with a paired control in each case.

### Corrections to v2

| # | v2 said | Why it was wrong | v3 |
| --- | --- | --- | --- |
| 1 | Add `SET LOCAL search_path = ''`, "free in the same statement" | MEASURED: it breaks vector search. `query.rs:4965` emits `$1::vector` and `<=>`/`<->`/`<#>` unqualified; `provisioning.rs:178-190` pins a role-level `search_path` so they resolve. Control: `0.008540`. With `search_path=''`: `ERROR: type "vector" does not exist`. | Pin to `pg_catalog` plus the confinement's extension schemas; never `''`, never the app schema |
| 2 | Read the epoch through a `SECURITY DEFINER` wrapper whose `app_id` "is validated inside the function" | The read runs *before* `SET LOCAL ROLE`, so `current_user` is the shared pool role for every tenant. There is nothing to bind the argument to; the sentence claims an authorization property it cannot deliver | No read wrapper. Plain schema-qualified `SELECT` as the pool login role, with the table unreachable by any app role |
| 3 | Follow "the shape `get_mask_policy` / `set_mask_policy` already use" | Those are the defect, not the pattern: `SECURITY DEFINER` + `GRANT EXECUTE TO PUBLIC` with no caller check (`bootstrap.rs:550-551`, `:589-590`), and `get_column_key(p_key_id)` is keyed by `key_id` alone (`:628-635`) | Do not copy them; fix them (defect register: L1, L2, and the reclassified v3 `get_column_key` finding) |
| 4 | "Delete the `set_mask_policy` `EXECUTE` grant from the app-role template" | The grant is `TO PUBLIC`, not to any template. Executing this instruction is a **no-op** and leaves every role able to write any app's policy | Drop the function; `REVOKE ... FROM PUBLIC` on its sibling |
| 5 | One batch: `BEGIN`, try-lease, epoch read, `SET LOCAL`s | A failed `pg_try_advisory_xact_lock_shared` returns `f`, a **normal result**, so the later statements still run and the epoch is read **without the lease** | Wrap the acquisition so failure aborts the batch with `55P03` |
| 6 | The dynamic callback "reads the referrer's id directly" from `host_defined_options` | Verified against the `Cargo.lock`-resolved v8 147.1.0: `data.rs:458-463` has no `impl_try_from! { Data for PrimitiveArray }` and no `is_primitive_array()`; only identity comparison exists (`:462`) | Per-runtime table of stamped handles, compared **by identity** |
| 7 | Lease key = "first 4 bytes of `sha256(app_id)`" with a fixed second key | 4 bytes is 32 bits. This is the **identical** birthday bound v2 criticised `hashtext` for, with the same constant second key | Single-bigint form, 64 bits of `sha256(namespace \|\| app_id)` |
| 8 | "Every transaction slot **and claim**" keyed by `(runtime_instance_id, tx_id)` | Unique transaction ids never contend. The existing app-keyed claim **deliberately** serialises two same-app top-level begins from before `BEGIN` through settle (`transaction/mod.rs:312-335`) | Separate registry identity from an explicitly chosen admission key |
| 9 | The SPI carries a policy-store capability | Contradicts v2's own section 10, which moves policy ownership to the control plane before the isolate exists | Deleted |
| 10 | The effective policy is "resolved before the isolate is built" | Resolved *once*. Lowering the operator ceiling then never reaches a pinned isolate. v2 traded a **forgeable** policy for a **non-revocable** one | Freeze only the declared half; resolve the ceiling at authorization time |
| 11 | Five state names constitute the transaction state machine | They are labels, not a protocol. Missing at minimum a `RollbackOnly`/`Poisoned` health state | Named as a required sub-contract |
| 12 | A creator transaction's lifetime is "bounded by a deadline enforced by the settle path" | Circular: no settle path runs for a body that never settles | Deadline enforced by an independent timer, in the sub-contract |
| 13 | Invariant 7: no data-plane path executes DDL | Contradicted by live code: `write_audit_unmask_row` runs on the **denied** path (`unmask.rs:410`) and the granted one (`:433`), and begins with `ensure_audit_unmask_table` -> `CREATE TABLE IF NOT EXISTS` | Delete the three lazy DDL sites; provision under the lease |
| 14 | Restore invalidates every live-cache entry "across all epochs" | Restore runs in a different process from the worker caches, and v2's own rule says correctness never depends on hint delivery | A 128-bit random epoch makes invalidation unnecessary |
| 15 | Delete the bare-specifier table | It has **three** arms; the third is `zeroship`, the creator-facing `env` facade, needed because the static BFS does not compile dynamically-only imports | Delete two arms, keep `zeroship` |

Nothing in this table is a reviewer's opinion; each was verified against the
code or measured.

### Where v4's corrections live

The table above is v3's mechanism and covers only v2's errors. **v4 corrects
itself inline instead**, in boxed or bolded passages that sit beside the claim
they retract rather than in a table at the front - so a reader who stops at the
table above will conclude this document has been corrected once, when it has
been corrected in both places. The inline ones include:

- section 8, `#### Location`: "Three gate arms, and the first is the one v3 got
  wrong" (the table-privilege arm), and the `CREATE SCHEMA` gate arm that can
  never fail because the real site interpolates the identifier;
- section 8, `#### One transition protocol`: "`p_expected` is load-bearing, and
  an earlier version of this signature omitted it"; "First provision needs its
  own arm, and an `UPDATE`-only CAS cannot serve it"; and the note that v4
  carried three mutually inconsistent versions of the signature;
- section 8, `#### Operation order`: "Two corrections to the sentence above,
  both verified, both mine" - the `search_path` pin, which could not be `''` and
  could not exclude `public`;
- section 9: "This table is WRONG - the real multiplier is ~17-19x"; the boxed
  reversal of the entry-count bound ("bound BYTES, or bound entries AND cap
  per-entry column count"); and the boxed reversal of deriving the metadata
  bound from the isolate bound;
- section 2 (the bootstrap hole in domain adoption), section 4 (the incarnation
  and domain missing from the binding identity), section 7 (the CDC producer
  claim asserted without checking), and the implementation sequence (step 3's
  neutrality claim, and 5b/5c printed in the wrong order).

This list is not a substitute for reading them: each retraction states what the
wrong claim was, which is the part that transfers.

---

## Required sub-contracts

Round 2's decisive finding was that v2 asserted contracts it did not contain.
The following six are **prerequisites for implementation**, not implementation
details. Each is a separate document or a named section, written and reviewed
**before** the code that depends on it. Until each exists, the corresponding
work cannot be given a failing test, which is the operative test of readiness.

| # | Sub-contract | Why it cannot be discovered under TDD | Acceptance shape |
| --- | --- | --- | --- |
| SC-1 | **Explicit transaction protocol.** States and transitions, resource ownership, cancellation, independent deadline, savepoint frames, effect buffer, terminal outcomes, and separate registry vs admission identity | A black-box suite passes on the wrong concurrency semantics. Whether two same-app top-level transactions serialise is user-visible and currently deliberate | A state table plus a test per illegal transition; explicit statements for "second same-app begin" and "settlement arriving while an operation owns the client" |
| SC-2 | **SQLite actor protocol.** Connection model, `Reserve`/reservation-qualified commands, cancellation acknowledgement, rollback and retire, release, and the fate of unrelated queued work | Two decisions change documented user-visible behaviour: whether autocommit work stalls behind an app's open creator transaction (today it does, `tx_route.rs:119-124`), and whether cancellation interrupts an in-flight statement or only inter-statement gaps | Concurrency arm: app A's autocommit **reads** proceed while A holds an open explicit transaction (**reads**, not ops - SQLite has one writer per database on any number of connections, so an "ops" arm cannot pass). Interrupt arm: cancellation takes effect *during* a long statement |
| SC-3 | **`DbPlan` IR and source ledger.** The plan/expression/projection/value/result/effect grammar, plus a checked construct-by-construct ledger from `query.rs` to its destination | `query.rs` runtime builders alone are ~3,100 lines (`:2907-6011`). An IR discovered incrementally will be shaped by whichever call site is ported first | A ledger whose source column is exhaustive and whose unported count reaches zero, checked by a gate |
| SC-4 | **Dev and HMR mechanism.** Supervised process restart versus runtime manager with listener swap; the artifact channel through `ServerOptions`; and whether the private module map applies in the dev vector | "A fresh dev isolate" names an outcome, not a mechanism. The server builds one runtime under one accept loop (`serve.rs:1719-1795`) | One observable restart/swap contract with a test that a removed collection is absent afterwards |
| SC-5 | **Service ownership.** `Arc<DbService>` at worker/CLI composition owning config, the stable thread-resource key, the process-wide live cache, the plugin prototype, and the neutral lifecycle handle | The plugin set is memoised **per thread**, not process-wide: `build_runtime` calls `plugin_set()` (`cache.rs:426`), which caches `create_plugins()` into a `thread_local!` (`:194-204`), so an n-thread worker still holds n plugin sets and a process-wide cache has no owner. (This cell said "calls `create_plugins()` inside each `build_runtime`" - true when written, fixed since in `22c4d75f1`, and retracted in SC-5 while this copy went stale. The per-runtime mint is gone; the per-thread scope is what SC-5 is actually about.) Deletion still reparses the URL and opens a second pool (`lib.rs:901,904`) | Current and pinned runtimes on one thread share exactly one backend slot and one cache, **and** a deprovision arriving while an isolate holds a handle has defined behaviour |
| SC-6 | **Ceiling read contract.** Where the app-current mask ceiling lives, who writes it, how a data operation observes a newly committed value, and that read's linearization point | A cache keyed by a version the reader can only learn by reading cannot discover a new version, so revocation would silently never arrive. Both round-3 reviewers reached this independently, from different angles - one saw no transport, the other no linearization | Lowering the ceiling denies the next `unmask` **by an actor the effective ceiling governs** in an already-built pinned isolate, with no rebuild and no deploy; the deny arrives **without** any invalidation message being delivered; **and the same test shows an unmask the ceiling still permits succeeds** - a deny-only arm passes on an implementation where the ceiling read is broken and everything is denied |

### The three forks, defined here because the sub-contracts cite them

`Fork A`, `Fork B` and `Fork C` are referenced by name in SC-1, SC-2, SC-5 and
SC-6 and were defined in none of them - review shorthand that leaked into the
contracts. They are:

- **Fork A - transaction admission on SQLite.** SQLite serializes top-level
  transaction admission per `(thread-resource, app_id, incarnation)`; the
  cross-isolate non-contention arm is **PostgreSQL-only**. Extra transaction
  connections are rejected: the tier has one isolate per app by construction and
  SQLite has one writer per database regardless.
- **Fork B - where authority is read.** An authority read **never traverses the
  data snapshot and never runs under the tenant role**. Autocommit rides the
  `prepare` batch (which precedes `SET LOCAL ROLE`); inside a transaction the
  read is on a separate platform-role session (PostgreSQL) or `op_conn`
  (SQLite), and may only ever **tighten** the value captured at `BEGIN`.
- **Fork C - what fences a stale handle.** A durable 128-bit `AppIncarnationId`,
  privileged-minted, qualified by the authority domain
  `(system_identifier, timeline_id)` so a PITR rewind cannot resurrect it, with
  permanent tombstones. **Epoch mismatch re-resolves; incarnation mismatch denies
  terminally.**

Two further contracts were open decisions. **Both are now made**, and this
paragraph previously still presented them as pending - so an implementer reading
only the parent would have re-opened questions the sub-contracts had settled:

- **A non-SQLite dev URL is typed-rejected**, not implemented (SC-4,
  Decision 1). It is not a fork, so its evidence and what the dev command does
  today are stated in section 10, SQLite, beside the same citation.
- **The deprovision/recreation lifecycle binds through a durable
  `AppIncarnationId`** qualified by the authority domain, with permanent
  tombstones (SC-5, Fork C). An epoch mismatch re-resolves; an incarnation
  mismatch denies terminally.

---

## TL;DR

`registerModel` is an asynchronous runtime handshake for work that no longer
belongs at runtime.

- PostgreSQL migrations are applied before a deploy becomes live.
- SQLite migrations are applied by the explicit development migration path.
- The JavaScript bootstrap already has the folded runtime descriptor needed to
  construct `env.db.<collection>` wrappers.
- Native CRUD still needs schema metadata, but a mutable, thread-local
  registration cache is the wrong place to hold it.

Replace the handshake with construction-time binding:

1. The worker resolves the content-addressed runtime descriptor and preserves
   the bytes `BlobStore` already verified, with their provenance.
2. The runtime transports it through a generic artifact bag;
   `zeroship-plugin-db` parses and semantically validates it before creator
   evaluation.
3. The plugin creates an isolate-owned `DbIsolateBinding` holding the immutable
   declared descriptor and a handle to service-owned shared resources.
4. A private, pre-user bootstrap module synchronously creates the JavaScript
   collection wrappers on `env.db`.
5. Each native operation takes a non-blocking shared schema lease, reads the
   database-resident schema epoch, and resolves one verified live-schema
   snapshot used for construction, execution, decoding, encryption, and masking.

There is no registration promise, no registration marker, no descriptor on
`globalThis`, no readiness gate, and no native method creator code can call to
alter runtime schema state or supply a security policy.

## Decision

Adopt an **automatic runtime DB binding** model, not a replacement registration
API.

`RuntimeSchemaDescriptor` remains part of the deploy artifact, consumed as
validated, immutable declared input during isolate construction. It is never
submitted through a creator-reachable V8 method.

| Authority | Lifetime | Responsibilities |
| --- | --- | --- |
| Folded runtime descriptor | Immutable for one code deploy/isolate | Collection allowlist, logical field facets, typed-ID prefixes, relations, indexes, SDK options |
| Live database metadata | Current for one app schema revision | Physical table and column shape, encryption sentinels, key/wrap metadata, masking classification |

Neither source is silently trusted for the other's job. A mismatch on a
security-sensitive field fails closed before data SQL is issued.

### Trust roots

- The **manifest** is the trust root for the runtime descriptor, and it is
  **unsigned**: `crates/zeroship-bundle/src/manifest.rs` carries no signature or
  attestation field, and `deploy_hash` (`:44-48`) is a digest the control plane
  computes on receipt. Content addressing proves only that the bytes match the
  hash that was requested. Nothing downstream may read "verified hash" as
  end-to-end integrity, and anything carried in the manifest is
  creator-authored.
- The **database** is the trust root for physical and security metadata, proved
  through an epoch the tenant cannot write.
- The **control plane** is the trust root for the mask-policy ceiling. Nothing
  inside an isolate contributes to it, and the ceiling is consulted at
  authorization time, not frozen at construction.

### Boundaries

The end state removes `zeroship-plugin-db`'s dependency on `zeroship-schema`,
which is then orphaned (its only `Cargo.toml` references are the workspace root,
plugin-db, and itself; AGENTS.md's claim that the migration engine reuses it is
stale and is corrected in the same patch).

Retirement is not a bullet point: `query.rs` alone is ~6,000 non-test lines
split between DDL/schema rendering (`:1019-2905`) and runtime query builders
(`:2907-6011`), and the crate also owns diff, descriptors, identifiers, FTS SQL
and the mask codec. One of those surfaces is security-critical:
`validate_collection`'s reserved-`__zeroship` prefix check (`query.rs:648-652`)
is the sole guardian of the namespace this design relies on. SC-3's ledger
enumerates every public surface's destination; retirement happens when the
unported count reaches zero, not when the dependency is dropped.

Module layout (a module split, not a crate split):

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

## Goals

- Make `env.db` complete before the first instruction of creator code runs.
- Remove all runtime schema-registration calls and readiness chains.
- Scope declared metadata to the isolate and deploy that owns it.
- Let pinned workflow isolates coexist with a current deploy on one thread.
- Make live physical and security metadata authoritative and cacheable by a
  database-resident, tenant-unwritable, never-reused schema epoch.
- Give PostgreSQL and SQLite the same metadata-resolution semantics.
- Keep migration application outside the data-plane runtime.
- Remove every creator-reachable privileged capability, including the ability to
  supply a security policy as an argument.
- Fail before SQL when metadata is absent, invalid, stale, or contradictory.
- Do not regress per-operation cost; the warm path gets cheaper, not dearer.

## Non-goals

- No runtime DDL and no replacement registration protocol.
- No change to the migration operation DSL.
- No new crates for PostgreSQL, SQLite, or the backend SPI.
- The runtime descriptor does not become the physical database authority.
- No post-launch migration or compatibility period.

In scope, having been out of scope in v1: the transaction settlement machine
(DBR-03), randomized-encryption atomicity (DBR-04/05), and the V8 decode budget
(DBR-06). v1 deferred all three while rebuilding the boundaries they live in.

## Invariants

1. **Creator code cannot mutate runtime DB metadata**, and cannot supply a
   security policy as an input.
2. **One isolate has one declared descriptor**, fixed for its lifetime.
3. **Code deploy identity and database schema epoch are different.**
4. **The database proves the epoch, and the tenant cannot write it.**
5. **One operation uses one metadata snapshot**, binding every path that returns
   row data, including CDC and live-query delivery.
6. **Security metadata fails closed.** Missing relations, unenumerable
   relations, malformed sentinels, introspection errors, or sensitive
   descriptor/live mismatches prevent data SQL.
7. **No data-plane path executes DDL.** This is enforced by *deleting* the
   lazy-DDL sites, not by a classifier that may never traverse them.

   **OWED: the enumeration itself.** This invariant and corrections row 13 both
   say "three", and between this document and the defect register exactly
   **one** is named - `write_audit_unmask_row`, which begins with
   `ensure_audit_unmask_table` -> `CREATE TABLE IF NOT EXISTS` and is reached
   from both the denied path (`unmask.rs:410`) and the granted one (`:433`).
   The other two are recorded nowhere in this document set. They must be
   enumerated against the tree before the deletions can be specified or the
   acceptance criterion tested; the count is not invented here.
8. **Raw JavaScript does not mean unverified plaintext.**
9. **No creator-reachable platform capability exists.**
10. **A private module is invisible, not allowlisted.** Secrecy of a specifier is
    never a security boundary.

## Architecture

### 1. Descriptor contract

`schema/runtime-db-descriptor-v1.json` is the language-neutral wire contract;
the TypeScript declaration is generated from it; the Rust serde representation
and semantic validator live in `frontend/runtime_descriptor.rs`. Conformance
fixtures prove the emitter, parser and binder accept and reject the same
documents. A present but invalid descriptor is an isolate load error and never
degrades to schema-less mode.

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

### 2. Plugin initialization

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
construct the binding identity below, and a plugin built from it would carry no
fence at all. It arrives on the version poll beside `deploy_hash` - the same
channel, since both are per-deploy facts the control plane owns and the worker
must not invent.

The authority domain is deliberately **absent** here. It is observed when the
authority row is first read, not supplied by the host, because a value handed in
by a caller cannot attest which cluster and timeline actually answered.

**That leaves a bootstrap hole, and it defeats the qualification precisely where
it is supposed to work.** A binding constructed *before* a PITR promotion, whose
**first** DB call happens *after* it, observes the new domain and adopts it as
its own - there is nothing older to compare against. If the same recovery
restored that app's previous `incarnation` row, both comparisons then succeed and
the domain-qualified token fences nothing at all. The fence is strongest against
a binding that has already read once and weakest against one that has not, which
is the opposite of what a bootstrap guard should do.

So the binding has two states, and the transition is part of the contract:

- **unbound** - constructed, never read. It may perform its first authority read
  and adopt the observed domain, **but only in this state**;
- **bound** - domain captured. Every later read compares, and a mismatch denies
  terminally.

The adoption itself must be fenced by something that does not come from the
database, or it is self-certifying. The worker's own identity supplies it: a
runtime instance is created after the process observed the cluster it was
configured against, so **a binding may only adopt a domain that matches the one
its `runtime_instance_id` was minted under**; a promotion between those two
points invalidates the runtime rather than silently re-homing its bindings.
Construction stays I/O-free, which SQLite requires, because the adoption happens
at first read rather than at construction.

Note the production gap this exposes: **nothing in production reads the domain
today.** `pg_control_system()` occurs exactly three times in the tree, all in one driver
integration test (`libs/compio-postgres/tests/suite/replication_live.rs:458-483`),
so step 6 must land the reader, not merely the columns.

`build_instance` becomes fallible. This requires host plumbing that does not
exist today: `NativePlugin::build_instance` currently receives only an app id
and is infallible (`plugin.rs:76-82`, `:224-235`), the worker passes a
descriptor string plus a global (`cache.rs:366-408`), and `ServerOptions` has no
artifact bag (`serve.rs:45-71`). That plumbing is part of the step, not a
detail.

### 3. Backend SPI

Neutral types: `DbBackendFactory`, `DbBackend`, `OpSession`, `DbTransaction`,
`PrepareRequest`, `DbPlan`, `DbValue`, `DbRows`, `LiveAppSchemaFacts`,
`SchemaEpoch`, route tokens, neutral `DbError`. One name per type;
`LiveCollectionFacts` is the per-collection slice of `LiveAppSchemaFacts`.

**Ownership.** A session owns its resources and carries no borrows. v1's sketch
returned a session borrowing the pool checkout and then declared `finish`
returning a `'static` future, which cannot compile. The choice is an **owned
pooled lease**, which requires `Pool::get_owned(self: &Rc<Self>) ->
OwnedPooledClient` preserving the borrowed wrapper's return and timeout
behaviour. `PgOpSession` drives raw `BEGIN`/`COMMIT`/`ROLLBACK`; it must **not**
store `Transaction<'_>`, which holds `&'a mut Client` (`transaction.rs:21-30`).

Raw transaction control carries an obligation the borrowing wrapper already
discharges: PostgreSQL may answer `COMMIT` with a `ROLLBACK` tag, which
`transaction.rs:54-59` detects and plugin-db's raw executor currently discards
(`backend/postgres.rs:201-211`). The session must inspect command tags.

**Explicit transactions** are a first-class owned object per SC-1, with registry
identity `(runtime_instance_id, tx_id)` and a **separate, explicitly chosen
admission key**. Those are different things: unique transaction ids never
contend, while today's app-keyed claim deliberately serialises two same-app
top-level begins (`transaction/mod.rs:312-335`, `context.rs:165-193`). v3 does
not silently delete that serialisation; SC-1 either keeps it under an
`(runtime_instance_id, app_id)` admission key or removes it deliberately and
specifies the resulting concurrency.

**Non-query capabilities**: CDC lifecycle (spawn, retained ownership, pause,
schema-pending, shutdown), key provision, audit insertion, operator lifecycle.
Each neutral, each with stated ownership and signatures in SC-3's ledger. Any
feature lacking one is deleted rather than left reaching for a concrete backend.

**`DbPlan`** is defined by SC-3. This document does not contain the grammar and
does not claim to.

#### What the SPI must cover

`BackendHandle`'s 78 textual references are not 78 operational callers. The
operational set is `exec.rs` (route selection, publication), `crud/mod.rs`
(dialect selection, search dispatch, key/encryption dispatch),
`crud/read_pipeline.rs` (key resolution, decryption), `crud/mask_policy.rs`,
`crud/unmask.rs` (policy/row fetch, key, audit), `crud/mask_drift.rs`,
`transaction/mod.rs`, `cdc_lifecycle.rs` (consumer spawn and retained
ownership), `register_model/mod.rs` (deleted), and `drop_namespace.rs`
(operator teardown).

**Most of those are not query shapes.** CDC lifecycle, key management, operator
deletion, backup/restore and persistent transaction ownership cannot be
expressed as a `DbPlan`, and v1's claim that they could is withdrawn. The SPI
carries explicit capabilities for CDC lifecycle, key provision, audit insertion
(insert-only, never DDL), and operator lifecycle. It carries **no policy-store
capability**: policy ownership is control-plane state resolved before the
isolate exists, so an SPI capability for it would reinstate the owner this
design removes. `SessionMinter` and the backup/snapshot contracts are either
assigned a destination in SC-3's ledger or deleted with the feature they serve.

There are also 19 direct thread-local `schema_for` reads plus a transaction-wide
enumeration, including six inside the concrete backends' search paths. Every one
is **replaced** in the step that deletes registration, not inventoried for
later: registration is still the writer for those caches
(`register_model/mod.rs:66-104`) and the readers span `context.rs:540-603`,
`:671-691` and `v8_classes/transaction.rs:66-90`.

### 4. Isolate-owned binding

```rust
pub struct DbIsolateBinding {
    // app_id, deploy_hash, runtime_instance_id, app_incarnation,
    // and the authority domain (system_identifier, timeline_id)
    pub identity: DbRuntimeIdentity,
    pub declared: Option<Arc<RuntimeSchemaDescriptor>>,
    pub resources: Rc<DbThreadResources>,  // obtained from Arc<DbService>, SC-5
    pub resolved: RefCell<IsolateResolvedMetadata>,
}
```

**The incarnation and the authority domain are part of the identity, not
decorations on it** - and until now this document did not mention an incarnation
at all while SC-5 required every binding to carry one. A binding keyed on
`app_id` alone cannot distinguish a handle cloned before a deprovision from a
handle belonging to the live app, which is the cross-incarnation hole SC-5
exists to close; and without the authority domain, a PITR rewind resurrects the
incarnation token along with the row that holds it.

The comparison is **terminal**, and this is the distinction that makes the field
necessary rather than redundant with the epoch: an **epoch** mismatch means
*re-resolve* (the schema moved), while an **incarnation** mismatch means *deny*,
permanently, with no re-resolution. One value cannot carry both meanings, which
is why the epoch cannot double as the handle fence.

Anchored in a typed V8 isolate slot and cloned into every native object,
transaction view, subscription helper and spawned future, so ownership is
structural. `IsolateResolvedMetadata` holds **exactly one epoch generation**:
observing a different route-read epoch clears it before repopulating. It is not
a view of the process cache; eviction there is invisible to a memo holder and
costs at most one re-resolution, and process entries are never pinned by isolate
references.

### 5. Private pre-user binding

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
- Identity mechanism: `compile_module` stamps a fresh `v8::PrimitiveArray` into
  `host_defined_options` (last parameter of `ScriptOrigin::new`, currently
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

### 6. Operation context

```rust
pub struct DbOpContext { collection: CollectionHandle, tx_route: TxRoute, input: OwnedDbOpInput }
```

`OwnedDbOpInput` is produced by a **total, fallible** decode: every
`Object::get`/`Array::get_index` returning `None` aborts with
`INVALID_ARGUMENT` (a key is never skipped, `v8_bridge.rs:250`; an element is
never defaulted to null, `:227-228`); a pending V8 exception is re-thrown; each
key is read exactly once so a `Proxy` cannot substitute values after SDK
validation; per-array, per-object, node and byte budgets are enforced before
allocation (subsuming DBR-06); non-finite numbers, functions and symbols are
rejected rather than coerced to null, because a filter value becoming null
changes the operator to `IS NULL`.

This is tenant isolation, not robustness: today a throwing getter on a filter
key is silently dropped, so `updateMany({tenantId: <throwing getter>, status})`
can execute as `WHERE status = ...` across every tenant's rows.

Pipeline: validate descriptor membership; singleflight lazy backend
initialization; `backend.prepare(request)` returning an `OpSession` (route +
bounded try-lease + epoch, one batch); reject `changing`; resolve descriptor
against live metadata; build and execute on the same route; decode/decrypt/
mask/normalize from the same snapshot; commit; emit success-only usage.

Construction happens after **epoch confirmation**, and may run speculatively
against the cached epoch's resolved metadata provided the plan is discarded when
the route-read epoch differs.

### 7. Resolved metadata

`ResolvedCollectionMetadata` carries the collection, epoch, physical facts,
optional declared metadata, and per-field `SecurityDisposition`
(`VerifiedPlain` | `Encrypted` | `Masked` | `EncryptedAndMasked`). Live metadata
governs tables, columns, physical types, encryption mode and key/wrap metadata,
mask sentinel/kind/classification, and sibling columns; declared metadata
governs the allowlist, typed-ID prefixes, logical JSON/date distinctions,
soft-delete and versioning, relations, and index declarations.

Resolution is collection-wide: one invalid field rejects the collection before
any data SQL.

| Declared | Live | Result |
| --- | --- | --- |
| Collection declared | Relation absent | `SCHEMA_NOT_APPLIED` |
| Collection declared | Relation present, relkind not enumerated | `SCHEMA_INTROSPECTION_FAILED` |
| Collection absent, descriptor exists | Any | `COLLECTION_NOT_DECLARED` before backend work |
| Field declared | Parent column absent | `SCHEMA_METADATA_MISMATCH` |
| Field absent | Extra live plaintext column | Excluded from projection, decode, writes |
| Field absent | Extra live sensitive set | Excluded; sentinels and sibling integrity still validated |
| Platform system field | Missing/incompatible | `SCHEMA_METADATA_MISMATCH` |
| Plain | Verified live plain | `VerifiedPlain` |
| Plain | Live encrypted or masked | `SCHEMA_METADATA_MISMATCH` |
| Encrypted/masked | Live plain | `SCHEMA_METADATA_MISMATCH` |
| Encrypted/masked | Same verified mode/key/wrap/kind/classification | Verified disposition |
| Encrypted/masked | Any sensitive facet differs | `SCHEMA_METADATA_MISMATCH` |
| Any | Malformed/orphaned sentinel | `SCHEMA_INTROSPECTION_FAILED` |
| Any typed field | Incompatible physical type | `SCHEMA_METADATA_MISMATCH` |
| Declared index | Differs or absent | Non-security diagnostic |

Backends distinguish "no such relation" from "relation present, not
enumerated": the PostgreSQL attribute scan restricts to `relkind = 'r'`
(`diff.rs:641`) while the platform elsewhere models `'r','p','v','m','f'`
(`bootstrap.rs:1648`) and publishes `'p'` (`publication.rs:20-21`). A relation
that exists but produced no attributes is `SCHEMA_INTROSPECTION_FAILED`, never
"absent" and never "plain".

The live code does not do this, and the gap is security-relevant rather than
cosmetic: a partitioned creator table's parent is `relkind = 'p'`, so it is
invisible to introspection, and "no metadata" is the same value as "no
protection needed". That is a pre-existing defect rather than a decision of this
design, and it is recorded with its evidence as **L17** in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`. The rule
above is what the fix must satisfy.

Malformed sentinel evidence becomes a typed error; `VerifiedPlain` is produced
only after the parser proves no valid, malformed or orphaned marker applies.
`None` must never mean both "verified plaintext" and "metadata unavailable".

**Delivery paths.** These rules bind every path returning row data. A change
event is resolved against a snapshot exactly as a read is: a `Masked` or
`EncryptedAndMasked` parent is replaced by its masked sibling and the sibling
key dropped; an `Encrypted` parent is dropped absent an authorization a read
would honour.

**The epoch is stamped onto the event at produce time, not read at delivery.**
The WAL tuple carries no epoch (`broker.rs:80-115`), but `ChangeEvent` is our
own struct and the producer is the mutation's own operation - which holds the
lease and already knows the epoch. Delivery then compares a stamped value
against the subscription's opening epoch with no I/O at all.

That distinction is load-bearing, and v3 got it wrong in the same way it got the
ceiling wrong: it said each batch is "resolved against the epoch current at
delivery", which the delivery path structurally cannot do. `publish` and
`deliver_event` are synchronous functions with no session, no pool and no
`async` (`broker.rs:565`, `:745`, `:836`); asking them to read the current epoch
is asking for a database round trip from a call that has no way to make one.
Stamping at produce is the only placement that a session-less delivery path can
honour.

**But "the producer is the mutation's own operation" is false on the path that
matters most, and this document asserted it without checking.** In production
the mutation-side producer is *suppressed*: `is_app_suppressed(app_id)` gates it
with the comment that when the WAL consumer runs for an app "it owns the publish
path for events this isolate writes", so "in production with the consumer
active, EVERY mutation previously paid the build cost only to discard the
result" (`crates/zeroship-plugin-db/src/exec.rs:455-466` for that reasoning; the
call itself is at `:501`). The real producer
there is `wal_consumer::emit_for_tuple`
(`crates/zeroship-plugin-db/src/wal_consumer.rs:589`) - which holds no lease, has
no operation context, and in which the string `epoch` does not appear **once**
in the entire file.

So the mechanism is well-founded on the local path and unimplementable on the
production one. The gap is real and none of the seven documents closed it: the
epoch has to reach the consumer **in-band in the WAL stream**, because the
consumer cannot ask for it any more than `deliver_event` can. Which carrier
does that - a replication message field, a per-transaction marker, or a column
the consumer reads off the tuple - is deliberately left open here rather than
guessed, but the requirement is not: **no design that stamps only at the
suppressed producer can ship**, and an acceptance arm that exercises only the
local path will not notice.

A stamped epoch differing from the subscription's forces a `Resync` before any
row is delivered, and an event whose metadata cannot be resolved is dropped,
closing the subscription with `SCHEMA_METADATA_MISMATCH`. The existing
schema-pending window (`broker.rs:790-796`, guard at `backend/mod.rs:1527`) is
the transition mechanism and currently has no production caller.

Generated SELECT lists contain only declared logical fields plus required
platform fields; never `SELECT *`, never a live-only column, and companion
columns are never creator-visible keys on any path.

### 8. Epoch and lease

#### Location

The marker is a row in
`__zeroship_admin.app_schema_state(app_id, state, epoch, incarnation,
deprovisioned_at, changed_at)`, owned by the platform role, granted to no
per-app role.

`incarnation` and `deprovisioned_at` are Fork C's, and this row carried neither
until now - the decision was adopted in prose while every concrete shape that
must feed or check it still described the old four columns. **`deprovisioned_at`
is what makes the tombstone a state change rather than a deletion**: the row is
never removed.

**What one rotating row does and does not give, stated precisely, because
"permanent tombstone" overclaims it.** The security properties hold: a stale
binding carrying incarnation A reads the current row, sees B, and denies
terminally; a delayed cleanup CAS naming A against a row holding B matches zero
rows and refuses. Both only ever compare against the *current* value, so one row
suffices to fence.

What it does **not** give is history. Once B is provisioned, the record that A
existed and was deprovisioned is gone, so the row cannot answer "was this id
ever retired, and when". If that history is wanted - for audit, or to reason
about a recovery after the fact - it needs an append-only companion, and this
document does not currently require one. Saying so is better than letting
"permanent" imply a durability the shape does not deliver.

The authority domain `(system_identifier, timeline_id)` is **not** a column - it
is a property of the cluster the row was read from, so storing it here would let
a restored dump assert its own provenance. It is captured by the reader at read
time and compared against the binding's. This is
forced by the live grant path: the reserved-prefix revoke exists only inside
`ensure_per_app_role`, which has **zero production callers**, while the live
provisioner (`apply.rs:1694-1707`) contains **zero `REVOKE` statements** and its
`ALTER DEFAULT PRIVILEGES` auto-grants full DML on every future
migrator-created table. A marker in the app schema would be tenant-writable, and
an epoch the tenant can rewrite proves nothing.

**There is no `SECURITY DEFINER` read wrapper.** The read executes as the pool
login role before `SET LOCAL ROLE`, so cross-tenant reachability is not a
property a function argument could establish - that connection legitimately
serves every tenant on the thread. The table carries `REVOKE ALL ... FROM
PUBLIC` and no grant to the app-role template, so no ordering can reach it from
an app role, and the read is a plain schema-qualified `SELECT`. Writes go
through `SECURITY DEFINER` `publish_schema_state` (signature in
`#### One transition protocol`), owned
by the platform role, `SET search_path` pinned in the body, `EXECUTE` granted
only to the migration/restore role and **revoked from `PUBLIC` in the same
statement** - the sibling wrappers in this schema are `GRANT EXECUTE ... TO
PUBLIC` and that pattern is deliberately not followed. If the ordering ever
moves after `SET LOCAL ROLE`, the read needs a wrapper that binds its argument
to `current_user`, not merely checks its shape.

**Three gate arms, and the first is the one v3 got wrong.** v3 gated `EXECUTE`
on `__zeroship_admin` *functions* - wording written when the design still had a
read wrapper. The property v4 actually relies on is **table** privilege absence,
so that arm would pass on a tree containing a single
`GRANT SELECT ON ALL TABLES IN SCHEMA __zeroship_admin TO <template>`. The arms
are therefore:

1. **Privilege absence on `__zeroship_admin.app_schema_state`, asserted at
   column granularity.** Deny-by-absence is the whole mechanism, so it is the
   thing that must be asserted. `has_table_privilege` alone is **not
   sufficient**, MEASURED on PG 16.14 with a paired control:

   | Grant state | `has_table_privilege(..., 'UPDATE')` | `has_any_column_privilege(..., 'UPDATE')` |
   | --- | --- | --- |
   | none | `f` | `f` |
   | `GRANT UPDATE (epoch)` | **`f`** | **`t`** |

   A tenant holding `UPDATE` on the `epoch` column alone therefore defeats
   invariant 4 while a table-level arm reports green. That is not a theoretical
   grant shape: column-level grants are house style one file from this table -
   `db/migrations-ts/20260818000200_worker_database_authority.ts:77,81,85`
   issues three of them.

   The arm therefore uses `has_any_column_privilege` as well as
   `has_table_privilege`, enumerates the **full** privilege list rather than
   `UPDATE` alone, and enumerates roles from `pg_roles` rather than naming the
   template - because the property must hold for roles that do not exist yet.
   Future roles need no special clause: every production
   `ALTER DEFAULT PRIVILEGES` in the tree is `IN SCHEMA <app|project|zeroship>`,
   none unqualified, so none can reach `__zeroship_admin`.

   **The arm also asserts the positive control**: the worker's login role *can*
   read `app_schema_state`. An absence-only assertion passes just as happily on
   a table nobody can read at all, including one that was never created or was
   dropped - which would take the entire data plane down while the gate stayed
   green. Absence and presence are asserted together or the arm proves nothing
   about the configuration that actually ships.

   (The starting position is genuinely deny-by-absence: the template holds
   `USAGE` on the schema with the in-source note that it "does NOT get any
   direct CRUD on admin tables", `auth/bootstrap.rs:168-178`, and app roles
   `INHERIT IN ROLE` that template, `apply.rs:1686` - which the arm must see
   through.)
2. The **ordering** is asserted: the epoch read is issued before
   `SET LOCAL ROLE` in the batch. It is load-bearing and was ungated in v3.
3. No `__zeroship_admin` `EXECUTE` is granted to `PUBLIC` or the app-role
   template. Full stop - the arm asserts the grant set, which is mechanical.

   v4 previously wrote this as "...unless the body binds its app-scoped argument
   to `current_user`", which is **not mechanically checkable**: no gate can read
   a PL/pgSQL body and decide whether it binds correctly. A conditional a script
   cannot evaluate is a comment wearing a gate's clothes, which is the exact
   shape this document criticises elsewhere.

   The escape hatch is made mechanical instead: any function that legitimately
   needs a broader grant is named in an explicit allowlist beside the gate, with
   its binding argument recorded, and the arm asserts the granted set **equals**
   the allowlist. Adding a function then requires editing the allowlist, which
   is reviewable; forgetting to reduces to a set-difference the script can
   compute.

**The epoch's entropy is the minting function's responsibility**, never a
caller's - the signature in `#### One transition protocol` gives a caller no way
to choose the bytes. That carries one caveat v3 omitted: a fresh random epoch
makes *minting*
collision-free, but `app_schema_state` is **itself restorable state**: a partial
recovery can reinstate an older row, and with it an epoch a worker cache still
holds. That is the same ABA this design rejects the migration digest for.

The primary rule is **mint before visible**: the recovery path publishes a fresh
epoch before the restored database serves traffic, which is what restore already
does. That covers app-scoped restore, where the service performs the operation.

It does **not** cover operator-driven PITR, where recovery happens outside this
system entirely. There the cache key is bound to
**`(system_identifier, timeline_id)`**, not `system_identifier` alone - a
distinction that matters and that an earlier draft got wrong.
`system_identifier` identifies the **cluster** (the tree reads it exactly that
way, via `pg_control_system()`, in
`libs/compio-postgres/tests/suite/replication_live.rs:477-483`), so a
same-cluster PITR *preserves* it and would alias straight back onto a live cache
entry. `timeline_id` is what moves when recovery rewinds and promotes. Both are
one cheap read - MEASURED together on PG 16.14 via `pg_control_system()` and
`pg_control_checkpoint()` - so the pair costs no more than the single value that
would not have worked.

`epoch` is a **128-bit random token** minted by `publish_schema_state`;
never-reused holds by entropy rather than bookkeeping, which is what makes
restore's cache story tractable below.

**`__zeroship_admin` does not exist in production.** `ensure_admin_schema` and
every installer are `#[cfg(any(test, feature = "test-helpers"))]`
(`bootstrap.rs:95-96`), no migration or SQL file creates it, and
`encryption/keys.rs:495` instructs operators to run a bootstrap migration that
does not exist. The epoch step therefore lands the **first production
provisioner** for that schema: a platform migration dated after the last row of
`db/released_migrations.tsv`, creating the schema, the platform role and
app-role template, the state table with `REVOKE ALL ... FROM PUBLIC`, and the
writer. The test-gated Rust installer is **deleted**, not un-cfg'd; two
provisioners for one schema is the defect this design removes elsewhere. A gate
arm asserts no Rust file creates that schema - **and it must not be a grep for
the literal `CREATE SCHEMA "__zeroship_admin"`, which is what an earlier draft
specified and which can never fail.**

That literal matches **zero** lines in `crates/` and `libs/` today, and the
schema is nonetheless created: the real site interpolates the identifier,
`format!(r#"CREATE SCHEMA "{ADMIN_SCHEMA}" AUTHORIZATION "{PLATFORM_ROLE}""#)`
(`crates/zeroship-plugin-db/src/auth/bootstrap.rs:145-150`). So the arm as
written passes on today's tree, passes after the deletion it is meant to
enforce, and passes if a second provisioner is added tomorrow using the same
idiom - it cannot observe its own subject.

The arm must therefore match the **interpolated** form (a `CREATE SCHEMA`
statement whose identifier resolves to `ADMIN_SCHEMA`), and it must be **proved
against a fixture containing the `format!` spelling** before it is trusted -
otherwise the next author reproduces exactly this blind spot. This is the third
of the three ways source enumeration fails, after shape and delegation, and it
is the one that leaves a green gate behind.

#### One transition protocol

There is **no transactional fast path**. The creator-migration boundary is
`zeroship-migrated/src/apply.rs` (`apply_ir_documents`); the platform runner
`migrate-adapter/src/platform.rs` has the same shape, and its only `BEGIN` is
the one-time ledger creation at `:611`, with its own comment recording that "a
crash between the engine apply and `insert_completion_ledger_row` leaves the gap
at the END" (`:951-953`). Neither can enlist in the vendored engine's per-step
transaction. A crash in that window leaves new physical schema under an old
epoch, so every later op hits the cache, resolves `VerifiedPlain`, and projects
a column a migration just converted to mask-only - fail **open**, defeated by a
crash rather than an attacker.

**This requires a signature change, and the change is the point.**
`apply_ir_documents` currently takes a **DSN** and opens its own session inside
(`crates/zeroship-migrated/src/apply.rs:235-236`, connect at `:437`), so a
caller has nothing to take a session-scoped lease *on*. "The lease is taken in
the caller" and "all on the same session" cannot both be true of today's shape.
The function therefore takes an already-connected session instead of a DSN, and
the caller owns the whole transition: acquire the lease, publish `changing`,
call apply **with that session**, verify, publish `stable`, release. Anything
less leaves the CAS fencing the publish while the DDL runs unfenced - which
reintroduces exactly the fail-open the protocol exists to prevent.

**The transition writer - one signature, stated once.** This is the SQL function
the caller commits `changing` and `stable` through at steps 1 and 4 below, not
`apply_ir_documents`:

```sql
__zeroship_admin.publish_schema_state(
    p_app_id        text,
    p_state         text,      -- 'changing' | 'stable'
    p_expected      text        -- the token the caller believes is current
) RETURNS text                  -- the token actually installed
```

This signature governs the **epoch** only. Two lifecycles that rotate on
different events - a schema change versus a deprovision - must not share a mint,
or a migration would silently issue a new app identity and terminally deny every
live handle. So the incarnation gets its own pair, stated here rather than
delegated to an unnamed function:

```sql
-- Deprovision: tombstone the row, never delete it. Idempotent.
-- The caller MUST name the incarnation it believes it is retiring.
__zeroship_admin.deprovision_app(
    p_app_id        text,
    p_expected      text        -- the incarnation the caller means to retire
) RETURNS text                  -- the incarnation actually tombstoned

-- (Re)provision: mint a NEW incarnation and clear the tombstone.
-- Fails if the app is live and not deprovisioned, so a provision cannot
-- silently rotate the identity of a running app.
__zeroship_admin.provision_app_incarnation(
    p_app_id        text
) RETURNS text                  -- the incarnation actually installed
```

Both are platform-role only, and neither lets a caller choose the bytes it
*mints* - the same rule the epoch already follows. `deprovision_app` sets
`deprovisioned_at` and leaves `incarnation` in place: the old value must remain
readable, because it is what a stale binding's terminal denial is compared
against.

**`p_expected` is load-bearing, and an earlier version of this signature omitted
it** - which looked finished precisely because the column and the two functions
were already there. Without it, `deprovision_app(app_id)` cannot distinguish a
*delayed* retirement of incarnation A from the live incarnation B that replaced
it, so a retry issued before a recreate tombstones the **new** app. That is not
hypothetical: the worker's pending-deprovision set stores bare UUIDs and
deprovisions by app id alone
(`crates/zeroship-worker/src/sync.rs:135-166`), and SC-5 explicitly requires a
cleanup carrying incarnation A not to act on B.

So the update is `WHERE app_id = ? AND incarnation = p_expected`, and a
zero-row result is a **typed mismatch**, not success. The check has to be inside
the function and atomic with the write: no cache invalidation, no terminal
comparison after the call, and no amount of care at the call site can repair a
tombstone that has already been written to B.

The caller does **not** supply the epoch: the function mints it, so entropy is
the function's responsibility and cannot be weakened by a caller. The caller
instead proves it still holds the transition by passing `p_expected` - the token
it read or wrote at step 1 - and the function performs one atomic
`UPDATE ... WHERE epoch IS NOT DISTINCT FROM p_expected RETURNING epoch`,
raising on a zero-row result. Two processes that both believe they hold the
lease therefore cannot both publish, and the winner learns the installed value
without a second read.

**First provision needs its own arm, and an `UPDATE`-only CAS cannot serve it.**
An `UPDATE ... WHERE ...` against an app that has no row yet matches zero rows
and therefore raises - so as written, the very first epoch for a new app can
never be installed, whatever `p_expected` is passed. `IS NOT DISTINCT FROM`
handles a NULL *epoch* in an existing row; it does not conjure the row.

The fix must not be a bare `INSERT ... ON CONFLICT DO UPDATE`, which would let a
caller with a stale `p_expected` overwrite a live epoch and would silently
destroy the mutual exclusion the CAS exists to provide - the tombstone in SC-5
depends on this row too. The insert arm is admissible **only** when the caller
asserts first provision (`p_expected IS NULL`) and only as
`INSERT ... ON CONFLICT DO NOTHING` with a zero-row result treated as a lost
race, so a concurrent provisioner loses cleanly instead of clobbering. Everything
else stays an `UPDATE` CAS.

v4 previously carried **three** mutually inconsistent versions of this - a
three-argument form in the location section, a four-argument form here, and a
sentence elsewhere saying the function mints the epoch while both signatures
passed it in. The signature is fixed here because the migration that creates the
function cannot be written without it. *(The surviving block sat under
`#### Location` when that correction was written; it now sits in this
subsection, beside the five steps that invoke it.)*

Under the exclusive lease, per batch, all on the **same migration session**:

1. commit `state='changing'` with a fresh transition token, before the engine
   runs;
2. run the batch;
3. reconcile the publication, then re-introspect and verify;
4. commit `state='stable'` with a fresh epoch, as a **compare-and-set on the
   transition token** so two processes that both believe they hold the lease
   cannot both publish;
5. release the lease.

A crash anywhere between 1 and 4 leaves `changing` and every data op fails
closed. A migration process that *dies* releases its session-scoped lease
automatically and leaves `changing` - safe. A migration process that *hangs*
holds it indefinitely, so the exclusive side carries its own deadline. A
partially-applied batch exposes `changing`, not "a valid epoch for the committed
prefix": that prefix is a schema no descriptor was folded against.

**Lock ordering, because there are three locks over one app.** The engine
already takes its own project advisory lock (`apply.rs:1053-1059`). The
exclusive schema lease is acquired **before** the engine is invoked and is never
acquired while the project lock is held; a gate arm asserts the lease is taken
only in `apply_ir_documents`' caller and in restore.

#### Acquisition

The data-side lease uses **bounded `pg_try_advisory_xact_lock_shared` retry,
never a blocking wait**. MEASURED on PG 16.14: a blocking shared request behind
a queued exclusive waiter stalls to `lock_timeout` (3105 ms) against a 118 ms
control; the try variant returns `f` behind a queued exclusive and `t`
otherwise, so it fails fast without starving the migration; and the three-way
cycle (op holds shared, waits on a row lock held by a transaction queued for
shared behind the migration's exclusive) does **not** deadlock - PostgreSQL
rearranges the wait queue and all three commit. There is no deadlock error to
map.

Blocking matters because a parked operation holds one of 8 pooled connections
(`lib.rs:862`) shared by ~200 apps per worker thread (`exec.rs:1273`).

A failed try-lock returns a **normal result**, so the acquisition is wrapped to
abort the batch rather than let later statements run unleased:

```sql
DO $$ BEGIN IF NOT pg_try_advisory_xact_lock_shared($k) THEN
  RAISE EXCEPTION USING ERRCODE = '55P03'; END IF; END $$;
```

An epoch value is therefore returned **only** when read under a held lease.
Retry policy is numbers, not the word "bounded": at most 4 attempts, 5/15/40 ms
jittered backoff, 120 ms total, then `SCHEMA_LEASE_TIMEOUT` (retryable). **The
pool checkout is released between attempts** - otherwise the retry loop
recreates in userspace the pool exhaustion try-lock was chosen to avoid.

The key is a single **64-bit** value, `sha256(namespace || app_id)` truncated to
64 bits, via the one-argument form. v2's "first 4 bytes of sha256 plus a
constant second key" is rejected: 4 bytes is 32 bits, the same birthday bound it
criticised `hashtext` for. One derivation function in `shared/lease_key.rs`
serves the data plane, the migration service and restore; a gate arm asserts
exactly one file constructs it. The `register_model` -> `schema_maintenance`
rename is load-bearing, not cosmetic - the tag is part of the key today, so it
lands atomically across all three or the shared and exclusive sides stop
contending while both keep passing their tests.

#### Operation order

One batch: `BEGIN`; the guarded try-lease; the epoch/state read; then the
per-app `SET LOCAL ROLE`, timeouts and `search_path`. Then reject `changing`;
use or populate the cache for `(authority_domain, app_id, incarnation, epoch)`;
build and execute; commit.

`search_path` for the per-app session batch is pinned to `pg_catalog` plus the
confinement's extension schemas - the same set `provisioning.rs:178-190` pins on
the migrator role - and never `''`, the app schema, or `pg_temp`. An empty
`search_path` is **not available**: the query builder emits unqualified pgvector
types and operators (`query.rs:4931-4932`, `:4965`). MEASURED: with
`search_path=''` a vector distance fails with `type "vector" does not exist`
against a control that returns `0.008540`.

Two corrections to the sentence above, both verified, both mine:

- **"never `public`" contradicted the rest of it.** The confinement's extension
  schema **is** `public`:
  `crates/zeroship-migrate-postgres/src/confinement.rs` sets
  `extension_schemas: vec!["public".to_string()]`, because "pgvector / PostGIS
  install into `public` on the platform/dev image". Both halves could not hold.
  `public` is on the path **for resolution only** - the app role holds no
  `CREATE` there - and the property actually wanted is stated directly: never
  the app schema, never `pg_temp`.
- **It is not "the same set the migrator role pins."** That pin puts the
  **project schema first** (`provisioning.rs:178-183`), which is precisely the
  entry the data plane must not have. Copy the extension-schema list from the
  confinement, not the migrator's `search_path`.

The cache key carries the full identity, not `(app_id, epoch)`. Dropping the
domain lets a PITR-restored cluster serve entries minted on the timeline it was
rewound from; dropping the incarnation lets a recreated app id read the previous
instance's cached metadata. Old keys may coexist until eviction - that is
unchanged - but **no binding can retrieve an entry whose identity is not its
own**.

The lease and epoch read run **before** `SET LOCAL ROLE`, as the pool login
role, so no per-app role needs any grant on `__zeroship_*`. Batch statements
execute in order, so the ordering is free.

#### Restore and app lifecycle

Restore and PITR replay are schema writers bound by the same protocol.
`backend/postgres.rs:1667` moves out of the data-plane crate into the migration
service - which today has no restore machinery at all, so the tooling
(`pg_restore` invocation, snapshot handle, blob access) is named work, not a
move. Under the exclusive lease it commits `changing`; performs the
drop/restore; **re-provisions the per-app role, its schema/table/sequence
grants, and both `ALTER DEFAULT PRIVILEGES ... IN SCHEMA` entries**
(`apply.rs:1699-1706`) - without the latter every table a *future* migration
creates carries no grant; **re-establishes schema ownership**, which
`pg_restore --no-owner` leaves on the restoring login role; **re-provisions the
three per-app platform tables** the `CASCADE` destroyed and the dump rewound;
re-runs publication reconciliation, without which subscriptions silently stop
forever; re-introspects and verifies; and only then publishes `stable` with a
freshly minted epoch, overwriting whatever the dump carried. Every failure arm
leaves `changing`; the current terminal state - release the lock with the schema
empty (`postgres.rs:1806-1821`) - is not acceptable under an epoch-keyed cache.

**Restore performs no cache invalidation.** It cannot: worker caches live in
other processes and the only channel is the hint path, whose delivery this
design says correctness never depends on. It does not need to: a fresh 128-bit
random epoch cannot equal a cached key, so every post-restore operation misses
and re-introspects. Open subscriptions need no separate teardown either - events
produced after the restore are stamped with the fresh epoch, so a subscription
opened under the old one sees the difference and resyncs. That only follows
because the epoch is stamped at produce; under v3's "read the epoch current at
delivery" it did not follow at all, since the delivery path cannot read
anything.

**Restore must also re-assert the mask ceiling.** The ceiling now lives in
`__zeroship_admin` beside the epoch, so it is one more piece of privilege state
the restore checklist owns - and unlike the schema cache, a stale ceiling is not
cured by a fresh epoch, because nothing keys a ceiling decision on the epoch. A
restore that reinstates a permissive ceiling silently re-grants unmask.

A restore that silently replaces the unmask audit trail with an older one is an
**audit-erasure primitive**; the rewind is recorded as an operator-visible
event.

App **deprovision and recreation** are bound by the same protocol: a recreated
app id must not inherit a stale marker, and deprovision must leave no row that a
later app of the same id would read as `stable`.

### 9. Metadata cost and the cache bound

What it costs to learn one app's live metadata, what it costs to hold it, and
the bound that follows. Where a figure below was later re-measured and found
wrong, the retraction is left in place beside it rather than folded away.

Three live defects found the same way constrain what this section may conclude,
and their evidence was extracted to
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md` rather than
restated here: **L17** (a partitioned creator table gets no runtime metadata,
so its encryption and mask passes are skipped), **L18** (the cold-start fix
opened a cross-tenant eviction DoS, and the window is open in shipped code
now), and **L22** (the same flat map is a per-transaction CPU cost, not only a
memory ceiling). Three more sit outside this plugin on paths this design
depends on - **L19** (the worker's env cache retains decrypted secrets),
**L20** (the meter's drain is a stop-the-world), **L21** (isolate admission
builds a runtime it cannot admit) - and a design that claims a bounded per-app
footprint while those are unbounded is claiming something the process does not
deliver.

#### The catalog read costs O(TOTAL TENANTS ON THE CLUSTER), not O(this app)

This is the finding that matters most for the stated target, and it is a
property of PostgreSQL rather than of our SQL.

`read_live_schema` filters on `WHERE n.nspname = $1`. **`pg_class` has no index
led by `relnamespace`** - verified on the project's PG 16 container, its three
indexes are `(relname, relnamespace)`, `(reltablespace, relfilenode)` and
`(oid)`. A leading-column mismatch means the planner cannot seek to one
tenant's tables; it scans and filters. The joined catalogs (`pg_index`,
`pg_constraint`, `pg_attrdef`, `pg_description`) have the same shape.

Measured independently on a throwaway PG 16.14 with synthetic app schemas:
going from 2,000 to 4,000 tenants **doubled** rows scanned and buffers touched -
`pg_index` 32,102 -> 64,102, `pg_constraint` 16,112 -> 32,112 - to return the
**same 112 columns**. Roughly 27.5 ms of catalog execution per cold-start read
at 4,000 tenants.

So the cost of learning one app's schema grows with **how many other apps share
the cluster**. At the platform's target that is not a constant factor, it is the
dominant term, and it is paid per `(app, deploy, worker thread)`.

**The fix in this branch removed one factor of three.** Cold start was
`N_collections x O(total_tenants) x N_threads`; populating every collection from
one read removes `N_collections`. The other two remain, and the `O(total_tenants)`
one cannot be fixed by caching - it makes each *miss* arbitrarily expensive as
the platform grows.

Closing it needs a different source, not a better cache. Options, none free:

- **filter by OID, not name.** Resolve the app's namespace OID once, then query
  `WHERE relnamespace = $1::oid` - `pg_class_oid_index` does not help either,
  but a covering index cannot be added to a system catalog, so this only helps
  where a join can be driven from the namespace side.
- **maintain the descriptor ourselves.** The migration engine already knows the
  schema it just applied; persisting the runtime descriptor at migrate time
  turns a catalog scan into a single-row keyed read of our own table. This is
  the option that actually scales, and it is a design change rather than a
  tuning knob.
- **shard tenants across clusters** so `total_tenants` per cluster stays
  bounded. Operationally real, but it caps the problem rather than removing it.

Recording the options rather than choosing: this is an architecture decision
with a cost the measurement now makes concrete.

#### Cold start introspects the whole app schema once per collection

Worse than the unbounded cache, and on the path that matters most at scale.

`runtime_schema_for` misses the deploy-keyed cache, calls
`read_live_schema(pool, app_id)`, narrows the result to the one collection it
was asked about, and caches **only that slice**
(`crates/zeroship-plugin-db/src/crud/introspect_schema.rs:96-110`).

`read_live_schema` is not a per-table query. It selects **every column of every
table in the app's schema** - `WHERE n.nspname = $1`, no table predicate - over
`pg_attribute` joined to `pg_class` and `pg_namespace`, `LEFT JOIN`ed to
`pg_attrdef` and `pg_description`, with a correlated subquery over
`pg_depend`/`pg_proc` per column
(`crates/zeroship-schema/src/diff.rs:606-640`).

So an app with N collections pays **N whole-schema catalog reads** on cold
start, each returning every column of every table, when **one** would populate
all N. The work is quadratic in collection count while the information needed is
linear, and the redundant reads are the expensive kind: catalog joins with a
per-column correlated subquery.

This is a cold-start cost, which is exactly the cost that dominates at millions
of apps: the long tail is rarely-hit apps, so a large fraction of requests
*are* cold starts. It is also invisible in any benchmark that warms one app and
then measures steady state.

The fix is small and needs no new mechanism - the single `LiveSchema` already
contains every table (`LiveSchema.tables` is keyed by table name), so the miss
path should populate the cache entry for **every** collection it just read
rather than discarding the rest. One read, N entries.

**That half is already committed and its other half is not, so it opened a
cross-tenant eviction window that is open right now.** One tenant's first
operation can insert one entry per table into a budget shared with its
co-residents, and the miss path has no singleflight. That is **L18** in the
defect register, with the measurement; the requirement it forces is the per-app
bound below.

Two things to get right while doing it, since both bear on the bound the next
subsection argues for:

- **do not cache internal tables** (`__zeroship_*`) as if they were creator
  collections - they would never be requested and would consume the budget;
- **one read, one deploy token**: every entry populated from a single
  `LiveSchema` must be stamped with the same token, or a redeploy landing
  mid-populate leaves entries from two schema versions under one identity.

#### Every per-app cache MUST be bounded, and today none of them are

"Until eviction", in section 8's `#### Operation order`, assumed a mechanism
that does not exist. Measured on the current tree, the per-app metadata maps on
`IsolateDbContext` have **no eviction at all** - counting
`remove`/`clear`/`retain` sites:

| map | eviction sites |
| --- | ---: |
| `introspected_schemas` | **0** |
| `deploy_tokens` | **0** |
| `schemas` | **0** |
| `mask_policies` | 1, a targeted per-app remove, not a bound |

**That census was incomplete.** Review found at least two more: `registered_models`
on the same context, and - the one that matters most - the **encryption key
cache**, whose module documentation states the property outright: "once a
`(app_id, key_id)` entry is inserted, it stays for the lifetime of the
`KeyStore`. There is no rotation surface today"
(`crates/zeroship-plugin-db/src/encryption/keys.rs:48-55`). It holds tenant key
material, 64 bytes per `AeadKey` before map and string overhead, for every
encrypted app the thread has ever served, and the `KeyStore` belongs to the
backend - which lives in the thread-local context until backend reset or thread
exit, **not** isolate eviction.

Counting four maps and calling it the census was itself the enumeration failure
this document keeps warning about: the list was assembled by grepping one
struct, so anything cached outside it was invisible to the count however careful
the count was over what it did cover.

**And that cache is a hot-path allocator, not just a leak.** Key resolution runs
**per encrypted column per returned row** on reads and per encrypted field on
writes - verified 2026-08-27 at `crud/encryption_pass.rs:335-336`, where
`backend.resolve_key(app_id, &key_id).await?` sits **inside** the
`for ... in to_decrypt` loop of a function invoked once per row. (Worth noting
how nearly this was retracted: grepping `\.resolve(` in `encryption_pass.rs`
returns **nothing**, because the call goes through the backend trait's
`resolve_key` wrapper, which forwards to `key_store.resolve` in
`backend/postgres.rs:1294`. One indirection was enough to make a true claim look
false - the delegation case this document's own enumeration warnings name.)

The lookup is
`cache.get(&(app_id.to_string(), key_id.to_string()))` (`keys.rs:316-329`) -
**two `String` allocations on every call, including cache hits**, because the
key is an owned tuple that `get` cannot borrow into. At millions of apps this is
the wrong shape twice over: it allocates per cell on the hot path, and it
retains key material per tenant forever.

The fix is unglamorous and certain: resolve the distinct `(app, key)` set once
per operation rather than per cell, make the lookup borrow instead of allocate
(a nested map, or a key type that implements `Borrow` for the `(&str, &str)`
pair), and bound the cache with zeroizing eviction keyed on the full identity.

They grow monotonically with the number of **distinct apps a worker thread has
ever served**, for the life of the thread. Nothing reclaims an entry when an app
goes idle, is evicted from the isolate cache, or is deprovisioned.

The isolate cache itself *is* bounded (`max_size`, LRU), which is what makes
this asymmetry easy to miss: an app is evicted from the isolate cache, its
runtime is dropped, and its schema metadata stays resident forever. The bounded
thing is visible; the unbounded thing sits behind it.

**At the platform's stated target - millions of apps - this is not a leak to
tidy later, it is a ceiling on apps-per-worker.** A worker's memory becomes a
function of how many distinct apps have ever routed to it rather than how many
are currently live, so capacity degrades with uptime and the only remedy is a
restart. CHWBL routing concentrates an app on a worker but does not stop the
long tail from accumulating there.

So this design owes:

- a **bound** on every per-app cache - entries or bytes, stated as a number the
  way the lease retry policy is, not the word "bounded";
- an **eviction policy** whose key includes the identity above, so evicting is
  never confused with invalidating;
- **eviction on deprovision**, which the tombstone already provides the signal
  for;
- and a **measurement**, not an estimate: the per-entry cost of a real
  introspected schema, times a realistic apps-per-thread figure. Nothing here
  should carry a byte number that was not measured on this tree.

**Measured** (`measure_cached_entry_size`, `crud/introspect_schema.rs`, run with
`--nocapture` to reproduce):

| shape | columns | serialized |
| --- | ---: | ---: |
| narrow | 8 | 273 B |
| typical | 16 | 551 B |
| wide | 40 | 1391 B |

**34 bytes per column, linear.** Three caveats that keep this figure honest:

1. It is the **serialized** size, and the cache stores a live
   `serde_json::Value`. That multiplier is now measured too
   (`measure_value_memory_overhead`, same module):

   | shape | serialized | in memory | ratio |
   | --- | ---: | ---: | ---: |
   | narrow (8 cols) | 273 B | 1,912 B | 7.0x |
   | typical (16 cols) | 551 B | 3,830 B | 7.0x |
   | wide (40 cols) | 1,391 B | 9,590 B | 6.9x |

   **7x, flat across shapes**, and the reason is structural rather than
   incidental: `sizeof(serde_json::Value)` is **72 bytes** and `sizeof(String)`
   is 24, so every column costs two `Value` slots and two `String` headers
   before a single byte of its name or type is stored. A schema is short keys
   and short values, which is the worst case for that overhead - the fixed
   per-node cost dwarfs the payload.

   This is still a **floor**: it counts what the type system fixes and excludes
   allocator rounding and fragmentation. Understating memory is the dangerous
   direction for a bound, so treat it as such.

   **This table is WRONG - the real multiplier is ~17-19x.** Three independent
   reviews measured it: two used a counting global allocator and got **17-18x**
   and **17-19x**; the third reproduced ~6.5-6.9x by repeating *this table's own
   structural method*, which is agreement about the method rather than
   corroboration of the number. The gap is the per-column `IndexMap`
   allocations the structural count never included. The workspace does enable
   `serde_json/preserve_order` (root `Cargo.toml`), so `Map` **is** an
   `IndexMap` and does carry per-map backing storage the structural count never
   included - which makes the higher figure mechanically plausible and this
   table's method the suspect one.

   The correction is not a rounding difference - it moves every derived figure
   by ~2.5x. The ceiling arithmetic below has been **re-derived from the
   allocator measurement** rather than annotated: 10,000 entries is ~100 MB
   (not ~38 MB), and 100,000 apps per thread is ~5 GB (not ~1.9 GB). The table
   above is left in place as the retracted evidence, not as a live figure; it is
   the only 7x number that should still appear in this document.

   Worth stating as a method note, because it is the more useful lesson: the
   structural count measured *what was easy to count* rather than what was
   allocated, and then reported it as "measured". A number with a unit and a
   test behind it still carries whatever the method left out.
2. The fixture columns carry no `encrypted` or `mask` facet. A real schema with
   those stores more per column.
3. It measures one collection. An app's total is that times its collection
   count, and after the cold-start fix a single read populates **all** of them
   at once - which is right for latency and means the whole app lands in the
   cache together rather than one collection at a time.

**What that arithmetic says about the ceiling.** Re-derived at the corrected
~18x multiplier, not the retracted 7x table: the typical entry is 551 B
serialized, so **~10 KB resident**. At a modest 5 collections per app one app
costs **~50 KB** of metadata. A thread that has served 10,000 distinct apps
therefore holds **~500 MB** it can never release; at 100,000 apps that is
**~5 GB per thread**, and a worker runs many threads. Nothing above is a
projection about traffic - it is the current code's behaviour multiplied by a
measured constant, and it is why the missing bound is a ceiling on
apps-per-worker rather than an untidiness.

The same arithmetic sizes the fix, and the unit matters - though **not in the
way this paragraph originally claimed; see the boxed correction below, which
shows an entry count does not bound bytes at all.** Taking the typical-shape
entry for scale only: a cap of 10,000 entries holds **~100 MB** at that shape,
which at 5 collections per app is ~2,000 apps resident per thread. That is a
number an operator can reason about, and it is also the number
that shows a per-entry cap alone is not generous enough to be uncontroversial:
whatever bound is chosen, the eviction policy will be doing real work under
normal load rather than sitting as a never-hit backstop, so it has to be
correct rather than merely present.

So the shape of the problem is: entries are **small** individually and the
count is what runs away. That argues for bounding by **entry count** rather
than bytes - a byte budget on ~500-byte objects is a more complicated way to
express the same limit, and it makes the eviction decision depend on a
measurement that drifts with schema width.

> **That recommendation is WRONG, and the fixture in this document is why it
> looked right.** Review measured a **25x spread** in bytes per entry across
> realistic shapes - because per-column cost depends on facets, not just column
> count: ~230 B/column for a plain column, ~400 B/column once `encrypted` and
> `mask` are present. So the same "10,000 entries" bound holds ~38 MB at this
> document's fixture shape, ~67 MB at a 16-column shape carrying facets, and
> **~476 MB** at 120 columns with facets. *(Those absolute figures come from the
> structural method this document already retracted; at the allocator-measured
> ~18x they are all ~2.5x higher again. The **ratio** is what matters here and
> is unaffected by which method produced it.)*
>
> **An entry-count bound admits a 25x range in the very quantity it exists to
> bound - so it does not bound.** And the counter-argument above ("a byte budget
> on ~500-byte objects") only holds if entries really are ~500 bytes; measured,
> they are 1.9 KB to 47 KB resident.
>
> The fixture is the cause of the error, and that is the transferable part:
> every column in it is a plain `text` with a short name and no facet, which is
> simultaneously the cheapest and the least representative shape. A bound
> derived from a fixture inherits the fixture's assumptions silently.
>
> **Resolution: bound BYTES, or bound entries AND cap per-entry column count.**
> Not entries alone.

#### Per-operation resolution and the shape of the map

Two per-operation costs sit behind this subsection - the cross-tenant scan
behind `mint_tx_view`, and the deep clone its caller discards, together with the
warm-path accessors that share their shape. Both are live defects in shipped
code rather than decisions of this design, so their measurements and citations
were extracted to the defect register as **L22**. What follows is what they
argue for, which is this document's to decide.

The corresponding commitment is small and worth stating as a rule: **per-app
metadata is immutable after resolution, so it should be stored behind `Rc` and
handed out as a cheap handle, resolved once per operation and threaded
through.** Not `clone()` per stage, not re-resolved per pipeline phase. Facet
flags and field indexes are computed when the entry is built, not rediscovered
by scanning on each use.

Both costs share one root with the memory ceiling and with the key-cache
finding above: **per-app state is keyed by string concatenation into one flat
thread-global map**, so every per-app question becomes a scan plus owned copies.
A hierarchical `app_id -> {collection -> schema}` map answers the name query in
one hash lookup, makes eviction-by-app a single removal instead of a prefix
sweep, and removes the `format!` allocation per call. The design should adopt
that shape rather than bolting a second index onto the flat one - it is the same
change that makes the bound enforceable.

**The bound should be derived from the isolate cache, not invented alongside
it.** The worker already bounds live isolates (`max_size`, LRU). An app whose
isolate has been evicted has no runtime that could ask for its metadata, so
holding that metadata is pure loss. Sizing the metadata cache to *the isolate
bound times a typical collection count* makes the two limits move together
instead of drifting apart - which is exactly how the current asymmetry arose:
one cache was bounded, the other was not, and nothing tied them.

The cleanest version of that is not a number at all but a **signal**: evict an
app's metadata when its isolate is evicted. That needs a path from the worker's
eviction to plugin-db, which does not exist today - verified 2026-08-27, the
only reference to plugin-db anywhere in `worker/src/cache.rs` is the
`DbPlugin::new` construction at `:219`; no eviction path calls into it.

> **Both paragraphs above are wrong in the same direction, and review caught it.**
>
> **1:1 coupling defeats the cold-start fix.** `max_isolates` defaults to **200
> per thread** (`worker/src/config.rs:129`). Under LRU churn - and under CHWBL
> spill oscillation, where the gateway walks the ring on saturation - isolate
> **evict-then-reload is the common case** at the platform's target scale, not
> the exception. Tying metadata lifetime 1:1 to isolate lifetime turns every
> reload into a fresh whole-schema catalog read, which is precisely the cost the
> populate-all change exists to remove. The design would spend a fix and then
> re-buy the problem through the eviction policy.
>
> **The correct relation is the inverse of what this section assumed.** A
> metadata entry is ~10 KB; a V8 isolate is orders of magnitude larger. So
> metadata entries should **outnumber** isolate entries, under an independent
> and larger byte-capped bound. **Isolate eviction is a good PRUNING HINT - it
> is not the right bound.** Sizing one from the other couples the cheap thing to
> the expensive thing's limit for no reason beyond tidiness.
>
> **And "the isolate bound times a typical collection count" is exactly the sin
> this section forbids two screens earlier**, where it demands that "nothing
> here should carry a byte number that was not measured". "Typical collection
> count" is an unmeasured multiplier, introduced by the same document that
> banned them. That is worth recording as a self-inconsistency rather than
> quietly deleting: the rule was stated correctly and then not applied to the
> next paragraph the author wrote.
>
> Mechanically the signal IS deliverable for the LRU arm - `evict_lru` runs on
> the owning thread, the same thread as the DB context - so a pruning hint is
> implementable. The deprovision arm is not, for the reasons in the census
> above. A hint that fires on one of the two paths is still worth having, as
> long as nothing depends on it for correctness of the bound.

SC-5's move of the live cache from per-thread to process-wide helps and does not
substitute for this: it replaces N copies with one, which is a constant-factor
win over thread count, while leaving growth proportional to distinct apps
unchanged.

#### Caches

The **epoch read** is route-bound. Live physical metadata is **not**: while any
shared lease is held no schema writer can commit, so the live schema for a given
epoch is identical on any connection, and equality of the route-read epoch with
the cache key is the validity proof. v1 mandated both same-route introspection
and a singleflight, which cannot both hold.

The live cache is process-wide (immutable plain data behind `Arc`, a
mutex-guarded sharded map never held across an await), owned by `DbService`
(SC-5). Its bound is stated in **both entries and bytes**, each re-derived from
a measurement of one representative app's `LiveAppSchemaFacts` before the epoch
step lands; v2's 256 was inherited from a per-thread design and never rescaled
when the cache became process-wide. Eviction removes the map entry regardless of
outstanding `Arc` holders. The **singleflight is per-thread**: a process-wide
one would need a cross-thread wake path that is unverified here, and the saving
is at most `n_threads` catalog walks per epoch bump.

### 10. SQLite

The explicit migration path remains the schema authority; runtime boot applies
nothing.

**The per-operation cross-process flock lease is deleted.** Every data operation
runs inside an actor transaction reading the epoch row as its first statement:
a deferred WAL transaction sees one stable snapshot, so invariant 5 holds
natively even while a migration process commits DDL; a write whose snapshot was
overwritten fails `SQLITE_BUSY_SNAPSHOT`, mapped to the retryable schema error;
`busy_timeout` bounds write waits. This deletes an entire file protocol, its
path vectors, its two-process conformance suite, and both of v1's flock problems
(per-open-file-description self-conflict, two-sided nonblocking-retry writer
starvation).

An exclusive flock survives for **restore's file swap only** - the one thing WAL
does not cover, since a lock release must not leave a connection bound to an
obsolete inode. The `:memory:` process-local guard is retained.

The actor is redesigned per **SC-2**, because the current one cannot honour the
cancellation and RAII contract this design depends on: commands carry only SQL
plus reply channels and still run after the receiver is dropped
(`session.rs:155-245`), one FIFO loop runs every route on one connection
(`:403-445`), and all handles clone that same actor (`:661-675`).

App attachment moves entirely into session preparation, reached by every path
that can touch an app file. `ensure_attached` validates app identity and
canonical path and opens the existing file with no-create semantics; a missing
file returns `SCHEMA_NOT_APPLIED` and a test proves no empty file was created.

Every path inserted into a SQLite `file:` URI is **percent-encoded**;
SQL-quote escaping is not sufficient (`session.rs:635` escapes for a string
literal then drops the path into a URI), so `?`, `#` or `%` in `db_dir` are
parsed by SQLite's URI parser and `?mode=rw` after an existing `?` is not the
mode parameter. Path vectors include one `db_dir` of each kind, each asserting
the resolved file and that `mode=rw` arrived as the mode parameter.

Which object holds the SQLite epoch, and which process writes it on each dev
path, is part of SC-4's decision and is still open there.

The dev-URL question beside it is decided, and this is where its evidence
lives: **a non-SQLite dev URL is typed-rejected**, not implemented (SC-4,
Decision 1). Today it is neither. The dev command always derives SQLite paths
regardless of scheme (`migrate-dev.ts:116-131`) and the addon exposes only
`applyIrSqlite`, so a PostgreSQL dev URL is neither rejected nor served: a
Postgres `DATABASE_URL` gets its migrations applied to a SQLite file while the
runtime is pointed at Postgres.

### 11. Mask policy

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

Replacement:

- The **declared half is untrusted and deploy-scoped**: manifest data, with the
  same standing as the runtime descriptor, since the manifest is unsigned. Its
  only security property is that it cannot outlive its deploy - which is the
  entire point of moving it out of durable storage.
- The **ceiling is live, not construction-time**, and its read is
  **authoritative per authorization** rather than cached behind a version the
  reader cannot discover. Only the declared half is frozen into
  `DbIsolateBinding`.

  **Inside an explicit transaction the ceiling is still read per
  authorization** - it is *not* pinned the way the schema epoch is. The two are
  pinned differently on purpose, and the distinction is the principle rather
  than an oversight:

  - the **epoch is pinned** for the transaction's lifetime because consistency
    demands it: one operation, one snapshot, and a transaction that saw two
    schema revisions would be incoherent;
  - the **ceiling is not pinned**, because it is *authorization* state, not
    schema state. Pinning it would let a long-running transaction hold a
    permissive ceiling across a revocation - reintroducing, inside the
    transaction, precisely the non-revocability this section exists to remove.

  **The read is NOT on the transaction's own connection**, and an earlier
  version of this paragraph said it was. Inside an explicit transaction the
  connection is running under the tenant's own role for the transaction's whole
  life - `apply_per_app_role` issues `SET LOCAL ROLE` plus the DB-1 guards
  immediately after the top-level `BEGIN`
  (`crates/zeroship-plugin-db/src/transaction/mod.rs:202-217`, called at
  `:540`) - and SC-6's privilege posture denies that role any access to the
  ceiling table. The read would return `permission denied`, and because
  "failure is denial" that would have shown up as a *passing* test while every
  non-`auto` unmask inside every transaction was bricked.

  So: **autocommit** rides the `prepare` batch, which this document already
  orders before `SET LOCAL ROLE`; **inside a transaction** the ceiling is read
  on a separate platform-role session and may only ever **tighten** the value
  captured at `BEGIN`. "Schema is pinned, authority is not" remains the rule,
  and SC-6 carries the full contract - including that an authority read never
  traverses the data snapshot and never runs under the tenant role.

  v3 said the ceiling was "control-plane state ... keyed and cached by
  `(app_id, ceiling_version)` under the same rules as the epoch". That is
  circular and is withdrawn: a cache keyed by a version it can only learn by
  reading cannot discover a newly committed version, yet the acceptance
  criterion requires the next `unmask` in an **already-built** pinned isolate to
  deny. "The same rules as the epoch" was also not transferable by assertion -
  the epoch is affordable because it rides an existing round trip, while
  `zeroship-plugin-db` has no HTTP client at all and
  `check_unmask_authorization` is a synchronous `fn` (`crud/unmask.rs:305`).

  Resolution: make the phrase literal. The app-current ceiling is a row in
  `__zeroship_admin` **beside the epoch**, written by the control plane and read
  in the **same batch** as the epoch read - its own schema-qualified `SELECT`,
  since it is its own table, riding the statement group the operation already
  sends. So it costs no extra round trip, it is observed rather than assumed,
  and its linearization point is the operation's own transaction.

  Two qualifications SC-6 carries and this summary must not lose: the read only
  precedes the authorization decision **after** the authorization point moves to
  after `prepare` (today it runs before any SQL,
  `crud/unmask.rs:1235-1236`), and the ceiling has a different home on the
  SQLite tier, where `__zeroship_admin` does not exist.
  Reuse the **shape** of the platform's existing operator-ceiling machinery -
  a versioned ceiling intersected with a creator-supplied value, as
  `crates/zeroship-migrated/src/policy.rs` and `policies/confined.policy.toml`
  already do for migrations - but **not its store and not its staleness rule**.
  An earlier draft said simply "reuse the vocabulary"; that was too loose in two
  ways, both verified:

  - That ceiling is **DDL-knobs-only**. Its key set is `CREATE TABLE` /
    `CREATE SCHEMA` / `RENAME` / destructive-ops / RLS (`policy.rs:32-35`), with
    no vocabulary for mask classifications - so sharing the store would put two
    unrelated policies under one name.
  - Its staleness response is **`ApprovalStaleCeiling` -> "re-submit required"**
    (`apply.rs:192-199`). That is right for a migration awaiting approval and is
    the **exact opposite** of what mask revocation needs: this document's own
    criterion is that a lowered ceiling takes effect "with no rebuild and no
    deploy".

  So: same idea, separate table, separate keys, deny-now rather than re-submit.
  **SC-6** owns the read contract and must state that difference explicitly, or
  whoever implements second will conflate the two ceilings.
  Resolving the intersection once at construction would make revocation
  unreachable for every pinned isolate already in the LRU - trading a forgeable
  policy for a non-revocable one.
- **No native function that writes a durable policy store exists in any
  isolate.** Delete `applyMaskPolicy` and `dispatch_set_mask_policy`; drop
  `__zeroship_admin.set_mask_policy` outright and `REVOKE EXECUTE` on
  `get_mask_policy` `FROM PUBLIC` - its **actual** grantee (`bootstrap.rs:589-590`,
  `:550-551`), not the "app-role template" v2 named, which would have made the
  instruction a no-op.
- There is consequently **no `maskPolicyReady` promise and no readiness gate**.

Delete the broad `DbPlatform` V8 class, its private slot, `__zsDbPlatform`, and
creator-facing replication diagnostics.

### 12. Transactions

Transaction views enumerate collections from the isolate's descriptor and clone
its binding. Each collection carries the exact transaction route alongside the
same identity and declared descriptor as its parent.

Everything else about transactions - states, transitions, health, ownership,
cancellation, deadline, savepoint frames, effect buffer, terminal outcomes, and
the admission key - is **SC-1**. This document does not restate five labels as
though they were a protocol.

Two constraints SC-1 must satisfy, both from current behaviour: settlement must
never interpret an absent client as proof that terminal SQL ran
(`transaction/mod.rs:986-997` does today), and cancellation must not drop the
client between `take` and the manual restore (`exec.rs:188-201`).

Randomized-encryption atomicity (DBR-04/05) is in scope and depends on SC-3's
plan variants: establish the conflict winner's stored id atomically before
encryption, preserve `predicate AND id` in the final mutation, and run the
multi-row algorithm in one internal transaction so behaviour does not change
with encryption mode.

## Error contract

| Condition | Code | Retryable |
| --- | --- | --- |
| Undeclared collection in a generated deploy | `COLLECTION_NOT_DECLARED` | no |
| Expected relation or schema state not applied | `SCHEMA_NOT_APPLIED` | no |
| App schema mid-transition | `SCHEMA_CHANGING` | no |
| Declared and live sensitive metadata disagree | `SCHEMA_METADATA_MISMATCH` | no |
| Introspection fails, is malformed, or cannot enumerate a present relation | `SCHEMA_INTROSPECTION_FAILED` | no |
| Bounded lease acquisition exhausted | `SCHEMA_LEASE_TIMEOUT` | yes |
| Logical metadata unavailable to raw JS | `DB_SCHEMA_REQUIRED` | no |
| Creator input not totally decodable | `INVALID_ARGUMENT` | no |
| App deprovisioned; its authority row carries a tombstone | `APP_DEPROVISIONED` | no |
| Binding's incarnation does not match the live one | `STALE_APP_INCARNATION` | no |
| Binding's authority domain does not match the cluster/timeline answering | `AUTHORITY_DOMAIN_MISMATCH` | no |

The last three are Fork C's, and the table carried none of them until now - the
fence was specified while the codes it must surface were missing, so the design
had no external behaviour for its own security decision.

**`APP_DEPROVISIONED` and `STALE_APP_INCARNATION` are separate on purpose,
because they occur at different times for the same handle.** Deprovision leaves
`incarnation` in place and sets `deprovisioned_at`, so a handle carrying A first
meets a tombstone bearing *its own* incarnation - that is `APP_DEPROVISIONED`.
Only once the app is re-provisioned, and the row holds B, does the same handle
fail on mismatch. Collapsing them into one code would make the audit trail
unable to distinguish "the app is gone" from "the app came back without you",
which are different operational events.

All three are **non-retryable**. That is the point of a terminal denial: unlike
an epoch mismatch, which re-resolves, none of these is improved by trying again.

`SCHEMA_CHANGING` is distinct from `SCHEMA_NOT_APPLIED` and its hint names the
**recovery** command. The distinction is load-bearing: the sibling code is on
the public allowlist precisely so creators can act on it
(`dispatch.rs:236-246`), so conflating them tells a developer to run migrate
against a database left mid-transition. `zeroship migrate --recover-schema-state
<app>` takes the exclusive lease, re-introspects, and compares against **the
migration service's own record**, not `"<app>"."__zeroship_migrations"`
(`audit.rs:227`), which lives in the app schema and is rewound by restore; after
a restore the verb refuses and names restore as the transition's owner.

## Implementation sequence

Dependency-ordered. v2's seven merges were inverted in seven concrete ways -
notably deleting the registration writer before replacing its readers, promising
CDC epoch behaviour before the epoch existed, and calling a step containing a
driver extension, an actor redesign, a compiler IR and a crate retirement
"mechanical and behavior-neutral".

1. Fix the live defects in
   `docs/proposals/2026-08-26-runtime-db-binding-defect-register.md` **that are
   genuinely independent**, each with its own regression test.

   "Fix every live defect independently" was the earlier wording and it cannot
   be followed: **L1, L2, L3 and L6 are coupled to later steps by their own
   intended end states.** L1/L2 end in deleting the policy writers with a
   manifest replacement, and L3 in deleting `__zsSchemaReady` - all three are
   step 5b. L6 *is* the absent production `__zeroship_admin` provisioner, whose
   first provisioner is step 7. Fixing them "independently" would mean inventing
   throwaway intermediate APIs, which the no-shim rule forbids. They are listed
   under step 1 as **diagnoses**; their repairs land where their end states do.

   Independent today: **L4, and only L4.** This line named four - L4, L5, L7,
   L8 - and three of them have since moved: L5 and L8 are marked FIXED in the
   register and are recorded as landed in step 2 below, and L7 was reclassified
   as a missing gate arm rather than a defect (register, "Reclassified from
   v3"). The count is re-derived from the register's status markers rather than
   carried forward.
2. **Landed.** Total decode shipped with `DecodeError`
   (`v8_bridge.rs:165`); L8's command-tag check shipped with
   `exec_terminal_on_tx` (`transaction/mod.rs:139`); the savepoint frame-effect
   fate shipped in `5b9bcbd49`, each with a regression test. Recorded as done
   rather than left as an instruction to redo work already in the tree.
3. Land `OwnedPooledClient` and the SQLite actor reservation/cancel/rollback
   primitives.

   **This step is NOT behaviour-neutral, and saying so was a contradiction with
   SC-2.** SC-2's two decisions deliberately change documented, creator-visible
   behaviour: two per-app connections let an app's autocommit *reads* escape its
   own open explicit transaction (retiring the `tx_route.rs:119-124` divergence),
   and cancellation begins interrupting a running statement. Both are the point
   of SC-2, not side effects of it. The step therefore owns that cutover
   explicitly - including the `docs/reference/sqlite-divergences.md` entry it
   retires - rather than claiming a neutrality that would have made the
   divergence land unannounced.

   **`OwnedPooledClient` is a capacity-model change, not a refactor, and this
   step must state which model it is choosing.** Today
   `acquire_dedicated_client` opens a **brand new TCP connection per
   transaction** - `compio_postgres::connect` directly, then a detached task per
   connection (`crates/zeroship-plugin-db/src/backend/postgres.rs:161-174`). It
   never touches the pool. So the current concurrent-transaction ceiling is
   *unbounded*, and a worker multiplexing ~200 apps that each open a transaction
   opens ~200 connections.

   Moving to a pooled checkout is right, and the reason is not tidiness: an
   unbounded ceiling is a way to exhaust PostgreSQL's `max_connections` from a
   single worker, which takes the cluster down for every tenant rather than
   slowing one. Bounded is strictly safer.

   But it inverts the failure mode, and the inversion is the part to design
   rather than discover: transactions that used to get a connection now
   **queue**, and the pool size becomes the concurrent-transaction ceiling for
   the whole worker. The data pool holds **8** (`lib.rs:862`). With SC-6's
   separate authority pool already carved out, the step owes an explicit
   answer to three questions:

   - what the transaction ceiling is, given it is now a shared resource across
     every app on the worker rather than per-app;
   - what a transaction does when the pool is exhausted - queue with the SC-1
     deadline, or refuse with a typed error. Queueing behind a deadline is
     preferable to refusal, but only if the deadline is shorter than the
     caller's patience;
   - whether one app can starve others, which the unbounded model made
     impossible and the bounded one does not.

   None of that is a reason to keep per-transaction connections. It is a reason
   not to land the change while calling it neutral.

   **DECIDED (operator, 2026-08-27): always use a pool.** Pooled checkout is the
   rule, not one option among several. That settles the direction and it reaches
   further than the transaction path, because three sites create connections
   today and only one of them is a pool checkout:

   - `lib.rs:862` - `Pool::connect(&url, 8)`, the shared data pool. This is the
     one that is already right.
   - `lib.rs:904` - `Pool::connect(db_url, 2)` **per deprovisioned app**. This
     uses *a* pool but builds a **new one per deletion**, so it pays two
     connects, two authentications and two TLS handshakes for work that should
     be a checkout from a long-lived platform-role pool. Under the rule, this
     changes.
   - `wal_consumer.rs:368` - `repl::connect_replication(...)`, a dedicated,
     non-pooled connection.

   **The replication connection is a genuine exception and should be recorded as
   one rather than quietly ignored.** A logical replication session is opened
   with the `replication=database` startup parameter and stays in streaming
   protocol mode for its entire life; it cannot serve an ordinary checkout in
   between, so there is nothing for a pool to multiplex. The rule that applies
   to it is not "pool it" but "**bound and account for it**" - which is exactly
   what L12 says is missing, since these are the connections whose cardinality
   hits `max_replication_slots` and `max_wal_senders` at ten.

   **What the decision does NOT settle**, and these still need answers before
   the step lands:

   - the **ceiling**: 8 is the current pool size, and it is now shared across
     every app on the worker rather than per-app;
   - **queue or refuse** on exhaustion - queueing behind the SC-1 deadline is
     preferable only while the deadline is shorter than the caller's patience,
     or a fast failure has been converted into a slow one;
   - **starvation**: nothing currently stops one app holding all 8. This is a
     NEW cross-tenant failure mode created by a change made for safety, and it
     wants an explicit mitigation - a per-app checkout cap, or fair queueing -
     rather than being discovered when one app's slow transactions stall its
     neighbours.

   The trade being made is real and worth stating plainly: before, one app could
   exhaust `max_connections` and take the cluster down for **every tenant
   everywhere**; after, one app can occupy the pool and stall **its co-residents
   on one worker**. The second is much better. It is not nothing.
4. Write SC-3: the normative IR, source ledger, and parity harness.
5. **5a (behaviour-neutral):** land the artifact/init channel and `DbService`
   (SC-5). Nothing creator-visible changes, so it lands on its own.

   **5b (the identity substrate):** the `__zeroship_admin` schema and its
   production provisioner, the `app_schema_state` row including `incarnation`
   and `deprovisioned_at`, the `deprovision_app` / `provision_app_incarnation`
   functions with their expected-incarnation CAS, the domain **reader** (nothing
   in production reads it today), and the wire that carries `app_incarnation` to
   the worker beside `deploy_hash`. This also resolves L6, whose end state is
   exactly this provisioner.

   It is listed **before** the cutover because the cutover's binding must carry
   this identity, and an earlier version of this sequence printed the two the
   other way round while calling one a prerequisite of the other. A step order
   that contradicts its own stated dependency is a defect in the plan, not a
   presentational quibble - an implementer follows the numbers.

   **5c (the irreducible cutover):** the private module map and binding, the
   SC-4 dev mechanism, replacement of every `schema_for` reader, and deletion of
   registration - co-landing manifest, SDK and docs. **This includes designing
   the replacement mask-policy wire, which does not exist**: deleting
   `defineMaskPolicy` removes the only way a creator can declare a policy, and
   neither manifest shape carries one (SC-6 states what the replacement owes).
   "Co-land the manifest" is not a packaging note here - it is a design task
   inside a step whose other work assumes it is already done. These cannot be
   separated: deleting registration breaks dev unless SC-4 co-lands.

   **OWED: no step in this sequence carries SC-6's masking storage flip.** SC-6
   records it as DECIDED - `ssn` stores the **masked** value, `ssn_raw` stores
   the real one, and `ssn_raw` cannot be filtered, projected or sorted on -
   which is what closes the filter oracle. No numbered step carries it, and
   before this note neither the flip nor `ssn_raw` appeared anywhere in this
   document. 5c is the closest fit, because the replacement mask-policy wire it
   already owns is the same subject. But **SC-6's four owed items do not all fit
   a runtime step**: constraints and indexes must follow the raw column, and
   `canonical_aad(collection, column, row_pk)` binds the physical column name so
   renaming it changes the tag - both are migration-engine work. A third is a
   creator-visible behaviour change (equality search by real value stops
   matching) that owes a `docs/reference/db.md` entry. Naming the gap rather
   than assigning it, because assigning it would decide it.

   Deleting registration removes **four** distinct effects, and the module's own
   header enumerates them (`register_model/mod.rs:1-35`) - an earlier draft said
   "three" by collapsing the first two, which are separate mechanisms with
   separate writers:

   1. **`cache_schema`** - the declared JSON that the 19 readers consume.
   2. **`mark_model_registered`** - a per-thread flag, written independently of
      the cache.
   3. **What that flag gates**, which is the security-relevant one:
      `runtime_schema_for` returns `None` for an UNREGISTERED collection
      (`crud/introspect_schema.rs:64-78`), so registration is what turns
      introspected encryption and mask metadata **on** for a collection's reads
      and writes. Absence of registration therefore currently means
      *unprotected*, which is the exact shape this design exists to delete - and
      it means the replacement must turn protection on by default, not merely
      relocate a flag.

      **This is worse than "an unregistered code path runs unprotected", and
      the missing half is what makes it reachable.** `db.collection(name)` mints
      a collection for **any non-empty string**, with no registration,
      descriptor or authority check at all
      (`crates/zeroship-plugin-db/src/v8_classes/db.rs:119-139`). The name is
      attacker-chosen, and it can name a table that a migration already created
      and masked.

      What that reaches is stated in the query builder's own documentation
      (`crates/zeroship-schema/src/query.rs:3000-3012`):

      ```sql
      -- baseline (schema=None or no masked columns):
      SELECT * FROM users WHERE id = $1

      -- schema declares ssn + email masked:
      SELECT "id", "ssn_masked" AS "ssn", ...
      ```

      `schema = None` yields `SELECT *`, which returns the **plaintext parent
      column** - the masked sibling substitution happens only in the `Some`
      arm. So schema-less app code that knows a masked table's name reads its
      plaintext, and the same absence turns the write pipeline's encrypt and
      mask transforms into no-ops.

      **Table-name secrecy is not a security boundary.** The consequence is not
      "protection is off until registration completes"; it is that protection is
      off for anyone who does not register, which is a choice the caller makes.
      The replacement must make *addressability* the thing authority decides -
      an addressable collection resolves live protection metadata or fails
      closed - because "not registered" cannot be allowed to mean
      "unprotected".
   4. **The SQLite `ATTACH`** (`register_model/mod.rs:122-129`, `:173-206`),
      which the module itself calls "in the wrong place, and that is a known
      item" - while direct SQLite paths still bypass the ordinary route that now
      attaches (`exec.rs:352-372`).

   All four replacements land in 5c or the step is not done.

   **5c cannot precede 5b, and an earlier version of this sequence had them the
   other way round.** 5c constructs the binding, and the binding must already
   carry `app_incarnation` and be checkable against the authority domain. Under
   the old order none of that existed yet: no admin schema, no mint, no wire
   field, and a SQLite construction path required to perform no I/O. The only
   ways to satisfy the step would have been a placeholder incarnation, a bare app
   id, or a lazy "adopt whatever row exists at first use" - and **each one
   reinstates the stale-handle and same-id recreation hole Fork C was adopted to
   close.** Building the fence after the thing it fences is not a sequencing
   preference; it is a window.
6. Land fail-closed live resolution for ordinary reads.
7. Land the epoch, leases, the writer/recovery/deprovision protocol, audit
   provisioning, and the dev migration paths, on the 5b substrate.
8. Land CDC source-epoch handling, per-subscription projection, and resync.
   **Blocked until the WAL epoch carrier is chosen** - the production producer
   has no epoch in scope, so this step cannot start on the strength of the
   local-path mechanism alone.
9. Land the owned transaction registry and state machine (SC-1) and randomized
    atomicity.
10. Port plan families and non-query capabilities. **Requires step 4 to have
    produced the normative types**, not the family sketch: SC-3 says in its own
    words that it is not a finished grammar, and it inventories only `query.rs`
    while this document hands it the non-query capability signatures too.
11. Delete `BackendHandle` and the thread-local caches, and retire
    `zeroship-schema`, when both checked inventories reach zero.

Generated declarations, fixtures, reference docs and gates **co-land with each
contract change**; repository policy requires every producer, consumer, fixture
and reference doc in the same patch. A final consistency sweep is not a place to
defer them.

## Acceptance criteria

Unchanged from v2 except where corrected, and stated as failing-test shapes.

**Boot and capability isolation.** Invalid descriptor fails before creator
evaluation. Creator top-level code reaches declared collections. Creator code
cannot observe, mutate or retain the descriptor transport. The three globals do
not exist. Creator imports of every `zeroship-internal:*` specifier fail **even
with the exact nonce**, through the static resolver, dynamic import, and
`__zeroshipNodeBuiltin`; that bridge is absent from production isolates.
Creator dynamic import of `@zeroship/db/internal` and
`@zeroship/bootstrap/install-schema` fails, while `await import("zeroship")`
still resolves. The bootstrap namespace exports only `default`. A reserved
specifier in the module list fails boot in **release** builds. No V8 method
registers a model, mutates declared schema, or writes policy, and no policy
value originating in an isolate is persisted.

**Fail-closed access.** Introspection failure issues no data SQL, asserted with
a counting executor. Missing metadata is observably distinct from
`VerifiedPlain`. Malformed sentinels fail on both backends. A relation present
but unenumerable yields `SCHEMA_INTROSPECTION_FAILED`, proved with a partitioned
table. A plaintext-to-masked redeploy cannot reuse a stale projection. A crash
between DDL and epoch publication leaves `changing`. A filter key whose getter
throws fails the operation and never produces a predicate that is a strict
subset of the declared filter.

**Delivery.** A mask-only field's plaintext never appears in a CDC event, WS
frame, or live-query payload; the test creates the column through a real
migration and reads a real WAL event - a hand-built `ChangeEvent` fixture does
not satisfy it. A subscription open across a migration receives a `Resync`
before any post-migration row.

**The epoch half of that arm is BLOCKED on the WAL carrier decision above and
must not be scheduled before it.** It requires a real WAL event to carry the
epoch, while the production producer has no epoch in scope and the carrier is
deliberately left open. Until the carrier is chosen the arm is unimplementable
as specified - and because the mask-plaintext half of the same criterion *is*
implementable, the whole arm can be reported green by a test that exercises only
that half. Split them, and let the epoch half fail loudly while the carrier is
undecided.

**Policy.** Lowering the operator ceiling denies the next `unmask` **by an actor
the effective ceiling governs** in an already-built pinned isolate, with no
rebuild and no deploy - **and an unmask the ceiling still permits succeeds in
the same test**, since a deny-only arm passes on an implementation where the
ceiling read is broken and everything is denied. SC-6 carries the full form; the
qualifier is repeated here rather than referenced because this is the criterion
an implementer reads first, and it was the unqualified version until SC-6 had
already been corrected twice.

**Cost.** A warm autocommit operation issues at most **three** server round
trips. `prepare` performs route acquisition, lease, epoch read and session setup
in one. This is pinned **after** the step that ports the plan families, not
before: the interval between the SPI cutover and that port would otherwise be
measured against a criterion the tree cannot yet meet.

The pool has **two** validation sources, not one, and both are excluded from the
count and asserted separately: a dirty checkout runs a validation `simple_query`
before use, and a *clean* connection idle beyond 500 ms pays an alive-validation
round trip (`pool.rs:1198-1208`). v3 carved out only the first.

Under a live migration holding the exclusive lease for a bounded interval, with
concurrent load from the migrating app and one other app through a single
thread's pool:

- the non-migrating app never observes `SCHEMA_LEASE_TIMEOUT`, and its p99 stays
  within a stated multiple of its no-migration baseline;
- **every operation of the migrating app fails within the retry budget, and none
  stalls toward the measured 3105 ms blocking figure** - this clause is the one
  that actually catches a regression to blocking acquisition, and v3 omitted it.
  Without it a blocking-wait regression passes: the non-migrating app fails with
  a pool-checkout error rather than `SCHEMA_LEASE_TIMEOUT`, and "no connection
  checked out during a backoff" measures an empty set when there is no backoff;
- a pool-occupancy instrument proves no connection was checked out during a
  lease backoff;
- **the first SDK retry after `stable` publishes succeeds**, with no process
  restart.

**Backend and boundary.** Equivalent resolved metadata across backends for one
logical fixture. No SQLite I/O during construction or binding. A missing app
file returns `SCHEMA_NOT_APPLIED` and creates no file. SQLite URL vectors
include `?`, `#`, `%`. Dropping a caller-side future races the actor
command's completion: cancellation winning interrupts, rolls back and retires
before acknowledging, completion winning yields `AlreadyCompleted` and claims no
rollback (SC-2). Not "cancels and rolls back" unconditionally, which cannot
pass - the actor may commit and reply before the caller polls.

**Module boundary.** `backend/api.rs` exists (it does not today) and no driver
types appear above it; one file names both backends; one file constructs the
lease key. `zeroship-plugin-db` has no `zeroship-schema` dependency and SC-3's
ledger has no unported entries. No data-plane path executes DDL - enforced by
the deletions, not only by a classifier. **This arm cannot be written until the
lazy-DDL sites are enumerated**; invariant 7 records that only one of the three
it asserts is named anywhere in this set.

## Live defects: moved

The numbered defects in the code this design touches live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`, with their
file:line evidence, their status, and - where a review got the mechanism or the
magnitude wrong - the correction. L1-L16 were extracted first; **L17-L22 were
lifted out of this document's architecture sections on 2026-08-27** in the same
pass that reorganised them, because each is a defect in shipped code rather than
a decision this design makes:

- **L17** a partitioned creator table gets no runtime metadata, so its
  protection passes are skipped (the governing rule stays in section 7);
- **L18** the cold-start fix opened a cross-tenant eviction DoS, and it has no
  singleflight;
- **L19** the worker's env cache retains decrypted secrets per app;
- **L20** the meter's drain is a stop-the-world;
- **L21** isolate admission builds a runtime it cannot admit;
- **L22** the same flat map is a per-transaction CPU cost.

They were extracted because they are the most perishable content here, and
because a register interleaved with the design made both harder to read and the
register harder to keep current. **No row has retired yet**: three carry FIXED
(L5, L8, L10) and one carries DECIDED (L9), which is a status change rather than
a removal. Re-derive that sentence from the register's markers rather than
carrying this one forward.

The ones that constrain THIS design rather than merely coexisting with it are
**L12** (the replication-slot ceiling, which puts the subscription transport in
question and is the open decision that **blocks** work here - not the only open
question in this set, but the only one that stops an implementer), **L16** (four
round trips and no
statement caching, which is the finding the IR most directly answers - a plan
has a stable shape, which is the precondition for a named prepared statement),
and the three section 9 cites by name: **L17**, **L18** and **L22**. The last
three are the reason a pointer is not enough on its own - the requirement they
argue for is stated in section 9's `#### Caches`, while the evidence that makes
it load-bearing now sits in a document designed to shrink.

## Rejected alternatives

**Rename `registerModel`** - preserves the wrong lifecycle. **Separate crates
per backend** - the problem is mixed ownership, not package count. **Keep
`BackendHandle` matches but move their bodies** - leaves every caller aware of
both dialects. **Worker or control-plane epoch polling as the freshness proof** -
always has a commit-to-observation window. **The applied migration-set digest as
the epoch** - repeats after rollback or restore. **Trust only the descriptor, or
only live introspection** - neither is sufficient alone. **Keep an asynchronous
schema-ready promise** - nothing asynchronous remains to represent. **Mutate
schema metadata during dev HMR** - a fresh isolate is a testable boundary.
**Store mask policy in the deploy descriptor only** - pins authorization to old
code.

**A blocking shared lease with a bounded `lock_timeout`.** Rejected on
measurement: a queued exclusive waiter stalls every later shared request for the
full timeout, and each parked operation holds one of eight pooled connections
shared by ~200 apps. The counter-argument - that try-lock converts an invisible
stall into visible retryable errors, and a retry storm could saturate the pool
it protects - is real, which is why acquisition is *bounded retry* with the
checkout released between attempts. If a later measurement shows p99 migration
batch time in milliseconds and the transaction deadline is in place, blocking
with a short lease-specific timeout becomes defensible again, and the acceptance
criterion should then pin a measured stall bound.

**A userspace cross-process flock lease for SQLite.** Rebuilds what WAL snapshot
isolation already provides, and brings its own starvation and
per-open-file-description problems.

**Process-wide singleflight.** Needs a cross-thread wake path unverified here,
to save at most `n_threads` catalog walks per epoch bump.

## Risks

| Risk | Mitigation |
| --- | --- |
| Bounded try-lease produces visible retryable errors during migrations | Bound the retry, release the checkout between attempts, make the code retryable, and measure the error rate against a real migration |
| Cold introspection latency | Introspect once per `(app, epoch)` with per-thread success-only singleflight |
| Resolution rules drift between backends | Resolution lives in `frontend/metadata.rs`; backends emit neutral facts and share parity fixtures |
| Migration crashes or hangs mid-batch | `changing` committed first, CAS publish, and a deadline on the exclusive side |
| Pinned code incompatible with new schema | `SCHEMA_METADATA_MISMATCH`; never stale metadata |
| The owned-checkout extension is larger than expected | It is a named step with its own parity tests, landed before anything depends on it |
| SC-1..SC-6 are written thinly and the same gap recurs | Each has a stated acceptance shape; the work they gate does not start until they are reviewed |
| Gate counts fall as registration code is deleted | Replace arms with end-state behavioural and absence checks, each declaring its ruled-on count and floor |
| **The suite the arms run in races, so no arm's verdict is trustworthy** | Fixed for `zeroship-plugin-db` on 2026-08-27 and worth stating as a standing risk, because it silently degrades every criterion in these seven documents |

## Verification methodology: moved

The account of how this codebase's tests report green while ruling on nothing -
five distinct mechanisms, each with a dated measured instance - now lives in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`.

It was extracted because it had grown into roughly a third of this document and
because it has a different lifetime: the defect register expires as fixes
land and this design will be rewritten once L12 is settled, while that record
stays true and applies beyond this design. The short form, which every
acceptance arm in this document set is subject to:

**An arm is evidence only if it was built, ran, and ruled on something.** Not
built, filtered out, skipped, aborted partway, and never scheduled are five
different ways of printing something other than a red.

## Documentation updates

`docs/reference/db.md`, `docs/reference/plugin-system.md`,
`docs/architecture/runtime.md`, `docs/reference/vite-plugin.md`,
`sdks/bootstrap/README.md`, relevant crate READMEs, and AGENTS.md's stale claim
that the migration engine reuses `zeroship-schema`. Each co-lands with the
change it describes.

The documentation should say: migrations define and apply schema ahead of
runtime; the folded descriptor installs bindings and supplies logical metadata;
live introspection verifies physical and security metadata; mask policy is
platform state, never an isolate input; and runtime DB access has no
model-registration phase.

## Final state

| Former effect | New owner |
| --- | --- |
| Build collection wrappers | Synchronous private pre-user binding |
| Hold declared logical metadata | Isolate-owned immutable `DbIsolateBinding` |
| Verify physical/security metadata | Database epoch + shared lease + live introspection |
| Select configured backend | The single `backend/factory.rs` composition point |
| Lower and execute a plan | The selected concrete backend |
| Attach SQLite app database | SQLite session preparation |
| Apply schema | Migration service / explicit dev migration path |
| Gate request readiness | Nothing; construction completes before creator evaluation |
| Apply mask policy | Control plane (ceiling, at authorization time) and manifest (declared half) |

Schema is applied before runtime, bindings are constructed with the runtime, and
every data operation verifies the live facts it depends on against an epoch the
tenant cannot forge.
