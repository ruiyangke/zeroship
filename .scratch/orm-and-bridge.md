Clean: ASCII only, no line-number citations. Full text below.

---

# The data ecosystem is an ORM and a bridge to V8. What follows from that.

Fourth document in the series. The first three are `.scratch/data-crate-reshape.md`,
`.scratch/wire-the-query-ir.md` and `.scratch/kill-test-helpers-plan.md`; a fourth,
`.scratch/data-plane-reconciliation.md`, reconciles the proposal corpus. Nothing here restates them.
Written at HEAD `7dd6ad9b5` (CONFIRMED), working tree carrying the authn changes in `git status`.
I ran greps and read source. I ran no `cargo` command, so every claim about compilation is
NEEDS-BUILD and carries the command that settles it. Naming is settled: the `zeroship-data-*`
prefix stays, `zeroship-schema` and `zeroship-plugin-db` keep their names, no crate is renamed here.

**One measurement note before anything rests on a count.** `grep` in this shell is a `ugrep` wrapper
forced into `-G`. `grep -cE '(^|[^A-Za-z0-9_])v8::' crates/zeroship-plugin-db/src/v8_bridge.rs`
returns `0`; `grep -c 'v8::'` on the same file returns a large number (CONFIRMED, both run this
pass). The same ERE form works for `app_id`. So the tenancy and V8 tables circulating in the review
material were produced with an instrument that silently returns zero on one of the two patterns.
Where a count is load-bearing below I give the plain-pattern command and no magnitude.

---

## 1. Does the frame hold?

**No, and the failure is not a mislabel, it is an instruction.** The frame is right about the bulk:
most of `zeroship-data-core`, `zeroship-data-engine`, the two vendor crates and the IR are an ORM,
and a thin, well-placed bridge lives in `zeroship-plugin-db`'s `v8_bridge.rs`, `tx_scope.rs`'s
continuation half and `v8_classes/`. It cannot hold two things. First, **multi-tenant isolation is
not a subject any module has, it is a property every module must hold simultaneously** in a process
where mutually distrusting tenants share one OS thread, one pool, one thread-local lane map, one
broker and one schema cache; and the frame's ORM verdict tracks the *absence* of tenancy, not its
irrelevance (CONFIRMED: `for c in zeroship-data-query-builder zeroship-schema zeroship-data-core
zeroship-data-engine zeroship-plugin-db; do grep -rl 'app_id' crates/$c/src | wc -l; done` against
`find crates/$c/src -name '*.rs' | wc -l` - the two crates that pass the frame's standalone-library
test cleanly are exactly the two with essentially no `app_id`, and every crate the frame files as
"ORM after modest parameterisation" is majority tenant-aware). Second, **a PostgreSQL logical
replication relay that has never been given a process** sits inside the V8 adapter; `zeroship-data-cdc-server`
exists, compiles, and refuses to start with `RELAY_UNAVAILABLE` (CONFIRMED). The damage is that the
ORM verdict arrives bundled with a directive - *move the hard-coded platform vocabulary into config*
- and applied to a module holding a tenancy property that directive converts a compile-time boundary
into a runtime argument. Two commits at HEAD did exactly that to the IR (`49e743269` deleted
`Ident::masked_sibling_of`, `7dd6ad9b5` deleted the platform-field union), each individually correct,
and `crates/zeroship-data-query-builder/src/projection.rs` records the residue in its own words:
the crate can make the protected form expressible but "cannot force the choice", and the schema-aware
layer that must force it does not exist.

**The corrected frame, which is what the rest of this document executes:** the data ecosystem is an
ORM, a bridge to V8, a multi-tenant isolation kernel spread across every tier, and a replication
relay with no process. And one precondition rides on the ORM verdict:

> A module is ORM only if its failure mode under parameterisation is *wrong results for one caller*.
> If the failure mode is *one tenant reaching another's data, connections, credentials or bill*, it
> is a tenant-isolation module regardless of its subject, it does not get the parameterisation
> instruction, and its property must be bound by a live gate before any file it lives in moves.

That clause is not decoration. There is no gate in `tests/` whose subject is cross-tenant containment
in the data plane (CONFIRMED: `grep -il 'tenant\|cross-app\|SEC-1' tests/*.sh` returns
`rls_binding_gate.sh`, which rules on platform tables in the `zeroship` schema, and `golden_path.sh`).
The properties at risk are held by unit tests inside the modules a reshape would move.

---

## 2. What is not the ORM and not the bridge

This is the eviction list. It has two halves and conflating them is the error the frame produces:
things that **leave**, and things that **stay and must be bound before anything around them moves**.

### 2a. Leaves

**The replication relay.** `crates/zeroship-plugin-db/src/{replication,wal_consumer,slot_reaper,cdc_lifecycle,change_stream_pg}.rs`,
plus `crates/zeroship-worker/src/slot_reaper.rs` and its supervision in the worker's `main.rs`, plus
`storage::ChangeStream` in `crates/zeroship-data-core/src/storage.rs`, plus the two suppression
registries in `crates/zeroship-data-core/src/broker.rs`, plus `DbLifecycle::deprovision_app`'s CDC
steps in `crates/zeroship-plugin-db/src/service.rs`, plus `v8_classes/replication.rs`.
*What it is:* a privileged consumer of a cluster-scoped named object with a retention obligation and
a garbage collector for objects whose owner died. Not mapping, not marshalling. The discriminator is
not the standalone-library test (these take a `&Pool` and an `app_id` and would compile anywhere) but
the privilege test that `tests/worker_replication_privilege_gate.sh` already measured: a
`NOSUPERUSER NOREPLICATION` login can read `pg_replication_slots` and take an advisory lock, and
cannot create a slot, drop one, or `START_REPLICATION`.
*Where it goes:* `crates/zeroship-data-cdc-server`, the decode half **rewritten** against
`zeroship-cdc-wire` rather than moved, because that crate's own `lib.rs` records that a verbatim move
compiles green and publishes into the wrong process's broker.
*What it costs:* the most, and it is not startable. `DatastoreId` exists in `zeroship-cdc-wire` and
nothing mints one (CONFIRMED: `grep -rn 'DatastoreId' crates --include='*.rs'` outside that crate
returns two doc lines in `zeroship-data-cdc-server/src/lib.rs`). `zeroship-cdc-wire` is in no
manifest anywhere (CONFIRMED: `grep -n 'cdc-wire' Cargo.toml` returns nothing). And the gate refuses
every partial move by arithmetic: `mints_total` is counted over the worker's whole normal-dependency
closure, so relocating a file between crates the worker links changes nothing, and removing the
reaper before `mints_total` reaches zero is refused. There is no gate-legal half measure.
*The part that is not blocked:* `Subscription.ready()` and `Subscription.next()` both call
`ensure_cdc_ready`, so a `for await` loop over `openSubscription()` mints a replication slot from
creator JavaScript (PLAUSIBLE by reading `crates/zeroship-plugin-db/src/v8_classes/subscription.rs`;
the gate's own header states the chain). Slots are `(app, worker)`-keyed against a
`max_replication_slots` this tree configures nowhere. **Do not "fix" that by provisioning eagerly at
boot or app-load** - `crates/zeroship-worker/src/cache.rs` churns isolates by LRU, so eager
provisioning converts a creator-triggered privilege into cluster-wide slot exhaustion whose failures
land on apps that never opened a subscription.

**The duplicate privileged provisioning plane.** `crates/zeroship-data-engine/src/auth/` (two files).
*What it is:* a second implementation of per-app role provisioning. `CREATE ROLE`, `GRANT`,
`ALTER DEFAULT PRIVILEGES`, `REVOKE` - operations needing CREATEROLE, which the worker must not hold.
*Where it goes:* nowhere. `zeroship-migrate-server`'s `apply::runtime_role_provisioning_sql` is the
production provisioner and already carries the same recipes. Delete.
*What it costs:* about forty fixture repoints, all mechanical, all in `zeroship-plugin-db`'s test
targets. `zeroship-data-engine` already declares `zeroship-migrate-server` as a dev-dependency, so
the edge exists. `tests/decision_four_gate.sh` already baselines these rows with the owner string
`Open 7 (delete auth/)`, so the repo ruled and nobody executed. The trap the test-helpers plan named
stands: `cargo check -p zeroship-data-engine --all-targets` sees almost none of the breakage;
`cargo check -p zeroship-plugin-db --features test-helpers --all-targets` is the command that does.

**Privileged tenant teardown.** `crates/zeroship-plugin-db/src/drop_namespace.rs`.
*What it is:* `DROP SCHEMA CASCADE`, `DROP ROLE`, `pg_terminate_backend`, `pg_drop_replication_slot`.
*Where it goes:* an endpoint on `zeroship-migrate-server`, which the module's own header already names
and defends. That is the smallest honest version of the missing teardown coordinator: no new process,
no new crate, `deploy/ops/Caddyfile` already routes `/v1/databases/*` there, and `provisioning.rs`
already owns the vocabulary.
*What it costs:* almost nothing today. The module is `test-helpers`-gated and ships in no binary, so
this is a placement correction with no exposure to close - but it removes those four statements from
a crate the worker links, which is the whole point.

**A dead operator capability.** `storage::Backup` and `capability::{SnapshotOpts, BusyPolicy,
SnapshotHandle}` in `zeroship-data-core`, with `impl Backup for PostgresBackend` in
`crates/zeroship-data-postgres/src/postgres.rs` and `impl Backup for SqliteBackend` in
`crates/zeroship-data-sqlite/src/lib.rs`.
*What it is:* `pg_dump` / `pg_restore` subprocesses, a cluster-wide advisory lock and
`DROP SCHEMA ... CASCADE` on one arm; a destructive file swap on the other.
*Where it goes:* deleted. Both impls are `#[cfg(feature = "test-helpers")]` (CONFIRMED, read this
pass) and every production `.snapshot(` / `.restore(` hit is in `zeroship-plugin-db`'s two integration
targets (CONFIRMED; the `held.restore()` hits in `native_transaction.rs` and
`collection.restore(filter)` are unrelated methods). The contract ships and the capability does not.
*What it costs:* ten call sites in two test files. This **replaces** the test-helpers plan's commit 6,
which gates it behind a new `backup` feature and says outright that it makes no ship-or-delete
decision. Gate it honestly and then delete and you pay twice, including for the `dep:sha2` routing
question that plan flags as needing its own build.

**The DDL and diff fork in `zeroship-schema`.** The DDL region of `crates/zeroship-schema/src/query.rs`
(between its `DDL builders for migration hosts` banner and its `Query builders` banner), plus
`compute_diff` / `ChangeKind` / `ChangeClass` / `DiffOp` in `diff.rs`, plus `cap_ident_name` and
`PG_MAX_IDENT_BYTES` in `ident.rs`.
*What it is:* a migration engine's DDL emitter and change classifier, for three dialects, one of which
has no backend in this tree.
*Where it goes:* deleted. `zeroship-migrate-core` already carries them.
*What it costs:* the classifier is free - it has zero production callers and its only live callers are
five sites in `crates/zeroship-plugin-db/tests/sqlite_integration.rs` (CONFIRMED:
`grep -rn 'compute_diff' crates libs --include='*.rs'` outside `zeroship-schema/src` and
`zeroship-migrate` returns one rustdoc link in `zeroship-data-core/src/storage.rs` plus those five).
The DDL region costs one enumeration before the commit is written, because three names inside it are
mask vocabulary the DML region reads and must stay: `RAW_COLUMN_PREFIX`, `raw_column_name`,
`declared_raw_column` and the sentinel comment builders. **And it costs a harness relocation first.**
`zeroship-schema`'s `mask_codec.rs` holds `cross_codec_parity`, `ident.rs` holds `engine_parity`, and
`query.rs` holds the three-way reserved-prefix set-equality arm. Those are the only comparators
binding this fork to the migration engine's copy, they live inside the crate being deleted, and the
2026-08-28 migrate-DSL divergence is what an unbound fork produces. Move them into a target that
outlives both copies and show them red under a deliberate one-side mutation *before* the deletion.

**Billing coupling.** `DB_READS`, `DB_WRITES`, `DB_ROWS_WRITTEN` in
`crates/zeroship-data-engine/src/metrics.rs`, and `zeroship-metering` as a normal dependency
(CONFIRMED, read this pass).
*What it is:* three `&'static str` identifiers the pricing catalog keys on, owned by the engine.
*Where it goes:* `zeroship-metering`, with the engine keeping a closed `OpKind` and an observation
port, and the mapping installed as an **exhaustive match** by `DbPlugin::register`.
*What it costs:* an instrument. Today `grep -rn emit_db_metric crates/zeroship-data-engine/src`
answers "what is billed" completely in one command over one crate; an opaque kind trades that for a
three-crate join. **If the exhaustiveness is not in the same commit, do not make the commit.** This
is the eviction I rank lowest and it is deliberately last.

**The charter loader.** `include_str!` of `policies/confined-system-shape.inject.toml` and `load()` in
`crates/zeroship-data-engine/src/system_shape_charter.rs`, with the `zeroship-migrate-policy`
dependency they keep alive. The `AssignmentPlan` / `AssignedColumn` types are ORM and stay;
`crud/system_fields_pass.rs` is already parameterised over them and is the proof the split is
achievable. `DbPlugin::register` already calls `stamp(plan)`, so only the lazy fallback has to go.
*Cost:* small, but it exposes a producer that must not be declared fixed by the move -
`zeroship-schema`'s `build_system_field_columns` renders a different column type for the same fields.

**Dead code, no argument.** `broker::ws_frame` in `zeroship-data-core` - a WebSocket push-frame
encoder whose every caller is past that file's `#[cfg(test)]` boundary, shipping in every binary, and
whose own comment records that its predecessor leaked raw masked columns unaudited. Delete.
`begin_ceiling` / `effective_ceiling` / `MaskCeiling` in
`crates/zeroship-data-engine/src/transaction/reducer/` - a mask fence nothing reads, because
`crud/unmask.rs` authorises off `mask_policy::cache_get`. It looks live in review and is asserted by
reducer tests, which is worse than absent. Delete.
Manifest dependencies the adapter never names: `aes-gcm`, `hkdf`, `hmac`, `zeroize`, `sqlite-vec` as
normal dependencies of `zeroship-plugin-db` (PLAUSIBLE by grep over `src/`; NEEDS-BUILD to be certain
cargo agrees: remove them and run `cargo check -p zeroship-plugin-db --all-targets --features test-helpers`).

**Two policies inside `op_error.rs`.** The file is correctly BRIDGE - it produces a
`zeroship-runtime` type a domain crate may not name - but it *stores* two decisions: which `DbError`
variants are retryable (encoded as which ones carry a hint, and pinned by an in-file test), and the
data plane's only HTTP status decision, `PermissionDenied` to 403. Give `DbError` `is_retryable()`
and `http_status()` in `zeroship-data-core` and let the lowering map shapes. Cheap; removes the reason
a second consumer of `DbError` would have to link the V8 crate to learn what is retryable.

### 2b. Stays, and must be bound before it moves

This is the half the frame has no arm for. None of these leaves; all of them are the reason a file
move is not free.

- Session containment: `pg_session_sql.rs`'s `SET LOCAL ROLE` batch and `pg_autocommit.rs`'s
  `roled_rows`, whose own tests are named for the property that a bare `SET` would ride the pooled
  connection to the next tenant. **This is the module the frame's parameterisation instruction would
  damage most**: turning the role derivation into a constructor argument makes `None` expressible.
- Lane routing and quota: `tx_lanes.rs`'s `HashMap<String, TxLane>`. The key does three jobs -
  invisibility, at-most-one-BEGIN, and the only *cardinality* bound on how many pooled connections one
  tenant can pin on one thread. `budgets` bounds duration, not count.
- Continuation binding: `tx_scope.rs`'s `enter` / `leave` / `capture_route`, and `tx_route.rs`.
- Cryptographic separation: the HKDF salt in `encryption/keys.rs`, and the AAD row-PK binding.
- Deploy-scoped schema authority: `schema_cache.rs`'s composite key and its no-`Option` `require()`,
  and `crud/protection_floor.rs`, whose whole reason to exist is that the descriptor is written by the
  process that runs creator code and the sentinels are not.
- Column-grant projection: `build_returning_expr` in `zeroship-schema`. The projection list the mapper
  emits *is* the authorization decision - `crates/zeroship-plugin-db/tests/column_grants.rs` holds a
  role with `INSERT` and not `SELECT` on a raw column, where `RETURNING *` fails 42501 and
  `RETURNING <cols>` succeeds. No ORM has this property, because no ORM's application connects as a
  principal narrower than its own schema.
- Per-tenant caps and attribution: `MAX_SUBSCRIPTIONS_PER_APP` in `broker.rs`, `metrics::handle_for`.

**The five tenant keyings in this tree disagree on purpose and must keep disagreeing.**
`schema_cache` and `protection_floor` key on app-and-deploy so a stale descriptor is unreachable;
`tx_lanes` keys on app alone or the connection quota evaporates; `metrics::handle_for` keys on app
alone or a redeploy resets a meter; `encryption/keys.rs` salts on app alone or every ciphertext
written by the previous deploy becomes undecryptable. That last one is irreversible data destruction
caused by making tenancy uniform. So the honest recommendation is **not** one `TenantId`: it is a
distinct type per keying discipline carrying its reason in its name, plus a gate asserting the five
stay different. If nobody will pay for that, write the five disagreements down and change nothing - a
documented asymmetry beats a half-executed unification.

And do not build that gate out of grep. Every property here is behavioural: that `SET LOCAL` reverts
on a cancellation-drop, that app B cannot claim app A's lane, that the salt actually separates
ciphertexts, that `RETURNING <cols>` executes under column grants where `*` does not. A shell-and-grep
gate goes green on all four while measuring spelling, which is worse than no gate.

---

## 3. What the ORM should be

**Internal shape.** Six crates, prefix unchanged, enforcement by manifest omission wherever possible.
`zeroship-data-query-builder` stays the zero-dependency statement AST and, after the IR migration, the
only producer of SQL text in the data plane. `zeroship-schema` shrinks to the shared vocabulary and
nothing else: the sentinel codec, the metadata value types, `descriptors.rs`, `SchemaName`, the
reserved-name tables, the raw-column naming convention, and the introspection result shape.
`zeroship-data-core` stays the contract floor and gains the SC-1 protocol vocabulary currently misfiled
in `error.rs` (`SettleIntent`, `TerminalResult`, `IsolationLevel`, `BeginIntent`, `OpenSessionError`,
`CleanupAck`, `SessionSetupDisposition`) as its own module - a file move with no dependency change that
removes the single largest reason `error.rs` reads as a dumping ground. The two vendor crates keep
their dialects and lose `Backup`, `mask_policy_store`'s write half, and (for PostgreSQL) the CDC
relay's departure. `zeroship-data-engine` stays the ORM centre and gains four ports the composition
root fills: `ProtectionOracle`, `UnmaskPolicy`, `AuditSink`, and a closed `OpKind` observation sink.

**What it is missing as an ORM, and this is the finding in this section.** Every mainstream ORM puts a
type/codec layer *below* the statement AST and consults it at three sites: value-in on write,
**operand-in on filter**, value-out on read. This tree has the write site (four passes in
`crud/write_pipeline.rs`), has the read site (`crud/read_pipeline.rs`), and **has no filter site**.
The consequence is live and squarely in the mapper: production contains exactly one
`aead::encrypt` call, in `crates/zeroship-data-engine/src/crud/encryption_pass.rs`, reached only from
the write pipeline (CONFIRMED: `grep -rn 'aead::encrypt' crates/zeroship-data-engine/src`). Nothing
encrypts a filter operand. Meanwhile `sdks/db/src/collection/encryption-fence.ts` explicitly *admits*
`$eq` and `$in` on a deterministic-encrypted column and refuses everything else, and
`sdks/db/src/types.ts` documents deterministic mode as enabling equality lookups (CONFIRMED). So the
advertised lookup either compares a plaintext operand against a mask, or against ciphertext - loud on
PostgreSQL, silently zero rows on SQLite. `docs/reference/db.md` contains zero occurrences of
"deterministic" (CONFIRMED), so the feature is advertised in the SDK and undocumented in the reference.
NEEDS-BUILD to settle end to end: a target driving `find({ ssn: <plaintext> })` against a
`t.encrypted({mode:"deterministic"}).mask({kind:"none"})` column on both backends, via
`tests/run_plugin_db_live_suite.sh` with `PG_TEST_URL` confirmed by the cancel-oracle print, plus
`cargo test -p zeroship-plugin-db --features test-helpers --test sqlite_integration`.

The fix is one funnel - lower every filter operand through the column its schema declares - and it
also deletes the hand-placed `maybe_lower_sqlite_boolean_filter` calls scattered one per verb in
`crud/mod.rs`, whose failure mode (a new operation forgets the call and is silently wrong) is the same
one `emit_db_metric` has. Do **not** import `TypeDecorator` unchanged: one logical field here becomes
two physical columns with coupled codecs (`mask_pass` reads a plaintext sidechannel `encryption_pass`
populates), so the seam is a two-phase codec - derive per field, place once per row - not a 1:1 chain.

**Is `libs/` right? Not yet, and the trigger is checkable.** `zeroship-data-query-builder` is already
`libs/`-shaped: its `[dependencies]` table is empty and it declares no `[dev-dependencies]` at all
(CONFIRMED). But it has one `ValueFormat` implementor, `PostgresValueFormat` (CONFIRMED:
`grep -rn '^impl ValueFormat' crates/zeroship-data-query-builder/src/`), and a query builder with one
backend is a PostgreSQL emitter with a trait in front of it. Each of `libs/compio-postgres`,
`libs/compio-redis`, `libs/compio-s3` solved a whole protocol before it earned the shelf. **Trigger,
all three:** two `ValueFormat` implementors pass one shared conformance corpus; the crate is in the
production dependency graph of a shipped binary (today its only real manifest edges are
`[dev-dependencies]` of `zeroship-plugin-db` and `zeroship-schema` - CONFIRMED); and its remaining
platform vocabulary arrives as constructor arguments, **exempting `MAX_PREDICATE_DEPTH` and
`RowLimit`'s ceiling**, which are a stack-safety bound and a type-level unboundedness property, not
quotas. Never move `zeroship-data-core`, the vendors or the engine: `zeroship-data-engine` cannot be
published at all while `system_shape_charter.rs` does an `include_str!` that escapes the crate
directory, and the boundary that already pays is `libs/compio-postgres`.

---

## 4. What the bridge should be

**What is left when it is only the bridge:** `v8_bridge.rs`, `tx_scope.rs`'s continuation half
(`scope_symbol`, `read_context_map`, `clone_context_map`, `current_tx_app`, `enter`, `leave`,
`capture_route`), `op_error.rs` reduced to a shape mapping, `v8_classes/`, `DbPlugin`, and the
composition root in `context.rs` and `service.rs`.

**What has to move out.** Eight of the twenty-two source files in `crates/zeroship-plugin-db/src`
contain no `v8::` token at all (CONFIRMED, plain pattern:
`for f in $(find crates/zeroship-plugin-db/src -name '*.rs'); do printf '%s %s\n' "$f" "$(grep -c 'v8::' $f)"; done`
- the zeros are `cdc_lifecycle.rs`, `change_stream_pg.rs`, `context.rs`, `drop_namespace.rs`,
`op_error.rs`, `replication.rs`, `service.rs`, `slot_reaper.rs`, `wal_consumer.rs`, of which
`op_error.rs` is legitimately adapter because `OpError` is a runtime type). Five are the relay, one is
the teardown, one third of `service.rs` is app deprovisioning. `context.rs` is the interesting case
and the answer is that it is **ORM connection management named after its host**: the `Db` v8_class
already carries owned Rust state in an internal field, so "V8 cannot hold a Rust handle" is false
here; the backend is thread-local because one backend serves every isolate on the thread, which is
pool sharing. It moves down with `ensure_backend` - **but only in the same change that reconciles the
lane key with `SchemaCache`'s**, because moving it puts two thread-locals with two key granularities
side by side in the engine with no local statement of the tenancy model that makes either correct.

**Two policies leave `op_error.rs`** (retryability, HTTP status), per section 2a.

**The falsifier for the whole eviction, and it is cheap.** After the relay and the deprovision third
leave, `zeroship-plugin-db` should no longer need `compio-postgres` as a normal dependency. If it
still does, something NEITHER is still in it. NEEDS-BUILD:
`cargo check -p zeroship-plugin-db --all-targets --features test-helpers` after removing the line.

**One live hole in the bridge's own fence, worth stating precisely because the frame calls this area
the safe part.** The DB-3 fence is applied in the bridge and *stored* in the engine:
`sanitize_app_actor` lives in `crates/zeroship-data-engine/src/crud/unmask.rs` and is called from five
production sites (CONFIRMED: `grep -rn 'sanitize_app_actor(' crates/zeroship-data-engine/src
crates/zeroship-plugin-db/src`, discounting the definition and the in-file tests - `crud/mod.rs`,
`unmask.rs` twice, and `v8_classes/masked_value.rs` twice). The placement is right: strip an untrusted
claim where it enters. The mechanism is not: `UnmaskFieldArgs` and `BulkUnmaskArgs` are public structs
with public fields in a public module re-exported through the adapter's ladder, so a sixth entry point
can construct a forged system actor and compile. And the no-policy arm of `check_unmask_authorization`
is literally `Ok(kind == "auto")` (CONFIRMED, read this pass) - the absence of a policy permits the
privileged kind for every classification.

---

## 5. The merged plan

Three plans now overlap: this eviction, the IR migration (`.scratch/wire-the-query-ir.md`) and the
`test-helpers` removal (`.scratch/kill-test-helpers-plan.md`). One ordering follows. Subjects are
conventional-commit shaped, one lowercase scope, imperative, ASCII, under 100 characters. SAFE means
shell, docs or manifest text only; everything else is NEEDS-BUILD with the command that settles it.

### Conflicts, and how they are resolved

1. **The IR plan's commit table is two commits stale.** Its commit 1 has landed and two thirds of its
   commit 11 has landed (CONFIRMED: `grep -rn 'masked_sibling_of\|MASKED_SUFFIX\|PLATFORM_FIELD_NAMES'
   crates/zeroship-data-query-builder/src/` is empty; `ProjectionSource::Stored`,
   `ProjectedField::stored` and `IdentRole::StoredColumn` are present). The third part did not:
   `field_is_readable` still lives in `crates/zeroship-schema/src/query.rs`. **Resolution:** strike the
   landed commits, promote the `readable: false` filter to its own commit. Its own plan warns that IR
   commit 15 reopens the hole without it.
2. **`Backup`: gate it or delete it.** Test-helpers commit 6 gates; this document deletes.
   **Resolution: delete.** Gating and then deleting pays twice and forces a `dep:sha2` routing question
   that never has to be asked.
3. **`auth/` is deleted by three documents.** **Resolution:** one commit, verified with the plugin-db
   command, not the engine's.
4. **`crates/zeroship-data-core/src/storage.rs` is edited breakingly by IR commit 18 and by the
   `Backup` deletion.** **Resolution:** delete `Backup` first. IR-18 then edits a smaller trait set,
   and IR-18 is the one commit that cannot be reordered later without corrupting data.
5. **"IR before the schema deletion."** True for the DML region, false as a blanket rule about
   `query.rs`. The DDL region and `diff.rs` are disjoint from the IR ladder, and deleting them early
   *shrinks* the file the ladder rebases across. **Resolution:** DDL and diff evictions run before the
   ladder; the DML region waits for it. Exception: the mask naming convention inside the DDL region
   stays.
6. **IR commits 5 and 6 duplicate test-helpers commit 3** (a SQLite leg and a live search-IR target).
   **Resolution:** one mechanism, the test-helpers version.
7. **Flag-first versus reshape-first.** **Resolution: instruments first, then reshape.** The
   test-helpers plan's own condition ("only if the re-cut actually starts") is satisfiable, because
   groups below are executable; but a reshape you cannot measure is one you cannot verify.
8. **Demoting `zeroship-core` out of `zeroship-schema` spends a property to buy a commit that may
   never land.** **Resolution:** do not demote until the migrate-backend collapse is scheduled with a
   date or refused in writing. See D3.

### The ordering

**Stage 0. Unbreak the checkout. One commit.**
- `fix(build): rebuild the sdk dist and retype the query bench on a schema name` NEEDS-BUILD.
  `crates/zeroship-runtime/src/core/bootstrap_modules.rs` `include_str!`s
  `sdks/bootstrap/dist/install-schema.js`, which is dated well before its own source and refuses
  anything but a v1 descriptor while the source refuses anything but v2 (CONFIRMED: both dates and
  both strings read this pass). And `crates/zeroship-plugin-db/benches/bench_query_build.rs` passes a
  `&str` to `build_find_with_schema`, which takes `&SchemaName` (CONFIRMED). Nearly every verification
  command below is blocked on one of those two.

**Stage 1. Instruments before moves.**
- `test(gates): rule on every live data target the suite already owns` NEEDS-BUILD. Test-helpers
  commits 1 through 3, collapsing IR 5 and 6. Four plugin-db live targets are named by no gate
  (CONFIRMED: `grep -rn 'search_ir_live' tests/ .github/` returns nothing).
- `docs(agents): correct the admin-schema and unmask-audit passages` SAFE. AGENTS.md still says a live
  statement names `__zeroship_admin`; `grep -rn '__zeroship_admin' crates db` returns nothing
  (CONFIRMED). It also says a rejected DB-3 claim is byte-identical to anonymous traffic;
  `SanitizedActor.rejected_claim` exists.

**Stage 2. Free deletions that shrink the IR rebase surface.** No ordering between them.
- `refactor(dbplan)!: fence the fields of a projected column` NEEDS-BUILD (section 6).
- `refactor(core): delete the unreferenced subscription push-frame encoder` NEEDS-BUILD.
- `refactor(dbengine): delete the mask ceiling nothing reads` NEEDS-BUILD.
- `refactor(db)!: delete the backup capability and its two implementations` NEEDS-BUILD:
  `tests/shipped_config_gate.sh` plus `cargo check --workspace --lib --bins`.
- `refactor(schema)!: delete the change classifier the migration engine owns` NEEDS-BUILD.
- `refactor(db): drop the manifest dependencies the adapter never names` NEEDS-BUILD.

**Stage 3. The protection surface. Order inside the stage is load-bearing.**
- `fix(dbengine)!: refuse an unmask when no mask policy is installed` NEEDS-BUILD. Regression test
  first: an unmask with `kind: "auto"` against an empty policy cache must be refused, and it passes
  before the fix.
- `feat(dbengine): fence the unmask argument types behind a sanitised actor` NEEDS-BUILD.
- `feat(dbengine): audit the actor claim an unmask refused` NEEDS-BUILD.
- `refactor(dbengine)!: take the unmask audit append through a sink port` NEEDS-BUILD.
- **Not in this stage: moving the mask-policy writer.** See D2.

**Stage 4. The harness, then the DDL eviction.**
- `test(gate): pin the mask sentinel codec across both implementations` NEEDS-BUILD, and shown red
  under a one-side mutation before the next commit. If it cannot be relocated, stop; the stage does
  not start.
- `refactor(schema)!: delete the ddl builders the migration engine owns` NEEDS-BUILD, after enumerating
  which DDL names the DML region calls.

**Stage 5. The ORM's missing layer, then the IR ladder.**
- `test(tests): refuse a silent zero-row read on a deterministic encrypted column` NEEDS-BUILD, red
  before the fix.
- `feat(dbengine): lower a filter operand through the column its schema declares` NEEDS-BUILD.
- `feat(dbplan): admit the declared column type vocabulary as literals` NEEDS-BUILD. Wider than the
  IR plan's single `TimestampMillis`: JSONB, DATE, INET and text arrays are all in `def_to_pg_type`.
- `feat(dbplan): add the sqlite value format` NEEDS-BUILD. Second implementor, and the first leg of
  the `libs/` trigger.
- `feat(dbplan): refuse a projection over an unreadable field` NEEDS-BUILD.
- Then the IR ladder as written, minus the landed commits. **IR commit 18 stays where its own plan put
  it**, behind both differential oracles: it deletes `SQLITE_BINARY_BIND_PREFIX`,
  `crud::sqlite_blob_param`, `encode_sqlite_binary_scalar` and both `SqlDialect` binary-bind methods
  together, and its failure mode is silent on the tier creators develop on.

**Stage 6. The duplicate privileged plane.**
- `refactor(dbengine)!: delete the engine's duplicate per-app role provisioner` NEEDS-BUILD:
  `tests/decision_four_gate.sh --self-test`, then a plain run, then
  `cargo check -p zeroship-plugin-db --features test-helpers --all-targets`.
- `refactor(migrate)!: move the namespace teardown behind the migration service` NEEDS-BUILD.

**Stage 7. `test-helpers` removal against the new boundaries.** The test-helpers plan's own
reshape-fragile set - its commits 7, 11, 14, 15 and 16's floor re-derivations - runs here and nowhere
earlier, because every one of them draws edges against crates that are about to lose a third of their
contents.

**Stage 8. The relay.** Blocked on D5. Mint a datastore record, declare `zeroship-cdc-wire` in the
workspace table with its first real dependent, invert `wal_consumer`'s broker coupling onto a sink
port, then move slot lifecycle and the reaper in one commit. Before any of it, fix `is_fatal`, which
substring-matches a `Display` that renders a server refusal as a bare string, together with its
disposition - changing the classification alone converts a detectably dead feed into an undetectably
partial one.

**Cut deliberately.** The metrics inversion, unless its exhaustive match ships with it. The
mask-policy writer move, per D2. Eager slot provisioning, always.

---

## 6. Start here

**`fix(build): rebuild the sdk dist and retype the query bench on a schema name`**

One commit. It is robust to every open decision in section 7 because it changes no design and takes no
position on any of them, and it is the precondition for the verification command of nearly every other
step in section 5: the runtime crate `include_str!`s a bootstrap bundle whose validator is a major
version behind its own source, and the plugin-db bench does not compile against a builder signature
that changed under it. Reclaim `.worktrees/lint-red/target` first, because `pnpm build` invalidates
`zeroship-runtime` and everything downstream.

```
pnpm build \
  && cargo check -p zeroship-plugin-db --all-targets --features test-helpers \
  && cargo test -p zeroship-plugin-db --features test-helpers --test sqlite_integration
```

The design work starts one commit later, and the only meaningful commit in this document that is *not*
blocked by the above is `refactor(dbplan)!: fence the fields of a projected column`: make
`ProjectedField`'s `source`, `alias` and `exposure` private with accessors (all three are `pub` today -
CONFIRMED) and leave `Ident`'s role taxonomy alone until its first projection-building caller exists.
`IdentRole::StoredColumn`'s reservation table refuses only the six classification names (CONFIRMED), so
a struct literal today names a raw column under the masked field's own alias, with no unmask
authorization and no audit row. Its verification is
`cargo test -p zeroship-data-query-builder`, which builds one crate with no dependencies, no database,
no V8 and no `pnpm build`.

---

## 7. The decisions only the owner can make

**D1. Unbounded `updateMany` and `deleteMany`.** Still open; it is D2 in `.scratch/wire-the-query-ir.md`
and that plan says no commit in the ladder should be written until it is answered.
`build_update_many_with_system_fields`, `build_delete_many` and `build_soft_delete_many_with_system_fields`
emit no LIMIT, while the IR's `write_bounded_target` emits its bound unconditionally and its own doc
says that is what makes an unbounded write unrepresentable.
*Options:* (a) truncate at the row limit, and a ten-thousand-row `updateMany` writes a few hundred and
reports success; (b) refuse, and a documented `db.md` verb becomes an error; (c) refuse only when the
caller supplied no explicit bound, and make `limit` a first-class argument on the two verbs.
*Recommendation: (c).* (a) is silent data loss on a write path and nothing recovers from it; (b) breaks
a documented shape that the reference itself uses as an example
(`deleteMany({ expiresAt: { $lt: ... } })`). (c) keeps the verb, makes the bound the caller's, and
after Stage 3's audit sink lands the refusal is a thing the platform can record rather than only throw.
Answer this before Stage 5's ladder starts.

**D2. Who owns the per-app mask policy?** Today the worker writes it, from inside the creator's isolate,
at boot, through `__platform.setMaskPolicy`, from the descriptor inside the `.zship` it is executing
(CONFIRMED by reading `crates/zeroship-data-engine/src/crud/mask_policy.rs`). By the privilege
invariant that is wrong: the process that runs creator code holds the write.
*Options:* (a) move the write to `zeroship-migrate-server` and `zeroship-migrate-sqlite`, both of which
already write mask sentinels and own an audit table; (b) leave the writer and write down that the
worker owns the policy as a decision rather than an accident; (c) (a) plus a policy arm on
`protection_floor` so a reader can tell "not yet written" from "written by a stale artifact".
*Recommendation: (b) now, (c) later, never (a) alone.* `zeroship deploy` and `zeroship migrate` are
separate verbs with no ordering constraint. Today the policy travels with the code, so cache and code
are the same artifact by construction. Move the writer and one order produces a support agent refused
by a 403 whose cause is an unapplied migration, and the other opens a live over-grant window before the
handler ships. **Do not move a writer across a service boundary until the reader can distinguish those
two states.** Stage 3's four commits are correct and independent of this decision; take them either way.

**D3. Does the migration engine keep zero platform edges?** The migrate family declares no
`zeroship-core`, `zeroship-schema` or `zeroship-data-*` dependency today, which is why the vocabulary
fork exists: it was refused, not forgotten. Collapsing it means `zeroship-migrate-backend` depends on
`zeroship-schema`, which first requires demoting `zeroship-core` there.
*Options:* (a) collapse the fork and accept the first platform edge in the migration engine;
(b) keep both copies and make Stage 4's parity gate permanent.
*Recommendation: (b), unless someone will schedule the collapse with a date.* (b) is a coherent end
state: the duplication becomes a priced decision with a comparator instead of a fork with a comment.
What is not defensible is demoting `zeroship-core`, landing the deletions, and leaving the collapse
unresolved - at that point `zeroship-schema` has been shaped for a consumer that never arrives.

**D4. `transaction/reducer/identity.rs`: wire the deploy fence or delete it?** The comparison is built
and the producer is missing - `expected_authority` mints a zero epoch and `observation_for` echoes it,
so `classify` compares a constant to itself. The adapter from a classified setup failure into a
retryable verdict now exists, so the epoch again needs only its producer.
*Options:* (a) wire it, with the record written by a service that does not execute creator code (the
control plane's app record gaining an incarnation, read through the registry poll the worker already
runs); (b) delete `LifecycleState`, `SchemaEpoch`, `AuthorityDomain` and the unreachable `DenyReason`
variants, and let the SQLSTATE-derived denial be the whole surface.
*Recommendation: (a), and if nobody will build the producer this quarter, (b).* Continuing as-is is the
only wrong answer: a total classifier documented as SC-1's central guarantee that cannot return
anything but "current" is a guard that reads as protection.

**D5. Does the relay get a process, and when?** It is the highest-hazard item in the pile and the only
one that cannot be started: nothing mints a `DatastoreId`, `zeroship-cdc-wire` is in no manifest, and
the gate refuses every partial move.
*Options:* (a) schedule the datastore entity and run Stage 8; (b) accept that the worker holds
`REPLICATION` for now and write that down, while taking the creator-reachable surfaces off the tenant
API; (c) both.
*Recommendation: (c), starting with (b) today.* Deleting `db.replication.watchdog()` from the V8
surface needs no privilege and moves no gate arm, so it is available now - but move the
`pg_replication_slots` read into the control plane in the same commit, because nothing else in the tree
surfaces slot lag per app. And configure `max_replication_slots` explicitly wherever this deploys; one
slot per app-and-worker against an unconfigured default is a scaling wall nobody has raised.

**D6. Does `zeroship-data-query-builder` move to `libs/`?** *Recommendation: not yet.* One dialect
implementor, no production dependent, and two breaking commits landed in it at HEAD. Move it when all
three trigger conditions in section 3 hold; never move the vendors, the floor or the engine.

**D7. `$ilike` on SQLite: refuse or emulate?** Open as D3 in the IR plan. *Recommendation: emulate,
narrowly.* SQLite's `LIKE` is already ASCII case-insensitive, so refusing `$ilike` breaks a documented
operator while leaving the undocumented equivalence in place, which is the worse of the two outcomes.
NEEDS-BUILD to confirm the default holds on the shipped build.
