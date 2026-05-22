# plugin-db docs audit — round 3 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + inline comments, plus
the `docs/reference/db.md` / `docs/reference/plugin-system.md` /
`AGENTS.md` surfaces that point into this crate.

**Prior rounds:** r1, r2 (cycle 01:17, 74/100). This pass re-audits
after the four follow-up commits the brief calls out:

- `e37b188f` — error.rs preamble fix + db.md path fix (r2 CRITICALs)
- `cbd12944` — new `orchestrator/lock_guard.rs`
- `5ceb6daa` — query.rs lowercase rewrite + wal_consumer visibility
- `dec2bd42` — migrations.rs restore of regression-reverted comments

**TL;DR.** The two r2 CRITICALs are cleanly closed. The new
`lock_guard.rs` preamble is excellent — fully documents the
Drop-cannot-await trade-off, the three exit modes, and the hand-off
contract. The `dec2bd42` restore correctly re-pins the regression
context in inline comments. Remaining drift is concentrated in two
classes the d53f90b0 sweep did not reach:

1. **v8_classes/transaction.rs** still references `crate::TX_CONN` /
   `crate::TX_TOKEN` as if they were live rustdoc-resolvable items
   (5 distinct intra-doc-link sites). They are not — the slots live
   on `IsolateDbContext` and the top-level symbols were deleted in
   Stage 8d-R4. These are real `[broken-link]` rustdoc warnings, not
   stylistic complaints.
2. **`docs/reference/plugin-system.md`** still describes a crate
   layout from 2025 (`src/callbacks.rs`, `src/validate.rs`,
   `src/migrate.rs`, `plugin-auth/`, `pg/`) — none of those paths
   exist on HEAD.

There are also two non-r2 type lies (validate.rs preamble's claim
about `SchemaRefused` not stamping `.code`; orchestrator/mod.rs's
"each submodule is `pub(crate)`") and one orphan TODO (composite
indexes — already implemented).

---

## Dimension-by-dimension findings

### 1. Stale historical names (TX_CONN / TX_TOKEN / callbacks.rs / MIG_LOCK / PENDING_EMITS)

```
[CRITICAL] crates/plugin-db/src/v8_classes/transaction.rs:68,161,192,214 — Rustdoc intra-doc links to `crate::TX_TOKEN` and `crate::TX_CONN`
  Why: These items were deleted in Stage 8d-R4 (folded into IsolateDbContext as `tx_token` / `tx_conn`). The `[`crate::TX_TOKEN`]` and `[`crate::TX_CONN`]` syntax tells rustdoc to resolve them — and they won't. Every one of these is a `cargo doc` broken-link warning that signals stale doc plumbing to anyone running it. d53f90b0 swept the prose but missed the bracketed intra-doc links.
  Fix: Replace `[`crate::TX_TOKEN`]` → `IsolateDbContext::tx_token` (no brackets, since it's pub(crate)); same for TX_CONN. The "formerly the TX_TOKEN thread-local" historical mention can stay if useful.
  Verification: grep -n "crate::TX_CONN\|crate::TX_TOKEN" crates/plugin-db/src/

[IMPORTANT] crates/plugin-db/src/v8_classes/transaction.rs:69-71,107-116,161,192,214-218,278,284-285,324 — Bare "TX_CONN" / "TX_TOKEN" tokens in inline comments
  Why: Even outside rustdoc brackets, these are nouns used as if they were the live symbol ("our token matches the current TX_TOKEN"). A reader greps and finds nothing; the wording implies a top-level constant. Twelve such sites in this one file.
  Fix: s/TX_TOKEN/IsolateDbContext::tx_token/ (or just "tx_token slot") replace-all; same for TX_CONN. Mirrors what mod.rs:17 (`v8_classes`) and orchestrator/transaction.rs:5 already do well.
  Verification: grep -n "TX_CONN\|TX_TOKEN" crates/plugin-db/src/v8_classes/transaction.rs

[IMPORTANT] crates/plugin-db/src/v8_classes/migration.rs:216 — `MIG_LOCK` thread-local referenced
  Why: "Delegates to `exec_fetch_batch` (which itself checks the `MIG_LOCK` thread-local for ownership / cancellation)." The thread-local was folded into `IsolateDbContext::mig_lock` in the same Stage 8d-R4. Single-site drift in this file but the comment will mislead the next reader chasing how cancellation is enforced.
  Fix: s/`MIG_LOCK` thread-local/`IsolateDbContext::mig_lock` slot/.
  Verification: grep -n "MIG_LOCK" crates/plugin-db/src/v8_classes/migration.rs

[MINOR] crates/plugin-db/src/lib.rs:110,216 — Doc comments still leading with the OLD name
  Why: lib.rs:110 says "Allocate a fresh non-zero TX_TOKEN value" and the test helper at lib.rs:216 is "**Test-only**: clear `MIG_LOCK` for the current thread". Both functions now manipulate IsolateDbContext slots, not standalone thread-locals; the all-caps names persist only because lib.rs:251 includes a "formerly TX_CONN" footnote that *some* reader will project onto neighbouring functions. Confusing.
  Fix: Either prepend "formerly TX_TOKEN" qualifiers consistently, or just retitle as "Allocate a fresh non-zero `tx_token` value." and "clear the migration-lock slot" — the constant-name idiom no longer reflects how the state is stored.
  Verification: grep -n "TX_TOKEN\|MIG_LOCK" crates/plugin-db/src/lib.rs

[MINOR] crates/plugin-db/src/crud.rs:53 — "the active TX_CONN" in run_op docs
  Why: Same drift class. Function-level doc on the most-called path in CRUD; reader trying to follow where the slot lives gets the wrong name.
  Fix: s/the active TX_CONN/the active tx connection (IsolateDbContext::tx_conn)/.
  Verification: grep -n "TX_CONN" crates/plugin-db/src/crud.rs

[MINOR] crates/plugin-db/src/exec.rs:329 — Test helper doc references "TX_CONN"
  Why: `exec_mutation_with_emit_for_tests` doc says "The caller is responsible for setting `TX_CONN`". The actual setter is `install_tx_marker_for_tests` which now installs into `IsolateDbContext::tx_conn`. Cross-references the wrong identifier.
  Fix: s/setting `TX_CONN`/installing the tx-conn slot/, or "(via `install_tx_marker_for_tests`)" alone (the function name already identifies the slot).
  Verification: grep -n "TX_CONN" crates/plugin-db/src/exec.rs

[MINOR] crates/plugin-db/src/orchestrator/transaction.rs:51,52,89,143 — Mixed "TX_TOKEN" / "TX_CONN" in inline comments
  Why: Same drift. The preamble (lines 1-14) does this correctly ("IsolateDbContext::tx_conn"); the inline comments below lapse back to the constant names.
  Fix: Mechanical s/TX_TOKEN/tx_token/, s/TX_CONN/tx_conn/ for the four sites.
  Verification: grep -n "TX_CONN\|TX_TOKEN" crates/plugin-db/src/orchestrator/transaction.rs

[MINOR] crates/plugin-db/src/backend/mod.rs:66 — "thread-local (e.g. `MigrationLock::client`, `tx_conn`)"
  Why: Both named items live in the per-isolate `RefCell<IsolateDbContext>` — they aren't standalone thread-locals any more. Trait doc, so externally visible to anyone reading rustdoc.
  Fix: "in the per-isolate context (e.g. `MigrationLock::client`, `IsolateDbContext::tx_conn`)".
  Verification: grep -n "thread-local" crates/plugin-db/src/backend/mod.rs
```

Verdict: the surface-level "appears in grep" hits have dropped from
~20 (r2 lens, before d53f90b0) to ~15, but the residue is now
concentrated in `v8_classes/transaction.rs` — the most-read file in
the wrapper layer. Worth a follow-up sweep narrower than d53f90b0
since the targets are localised.

### 2. Tense drift / in-flight migrations that already shipped

```
[OK] No future-tense preambles claiming an in-flight refactor that has shipped were found.
  Spot-checked: error.rs (post-e37b188f — accurate "sweep is pending" with concrete known sites), lock_guard.rs (post-cbd12944 — past-tense "Three commits in two days plugged…"), validate.rs (the Result<_, String> hold-out narrative is still factual — see Dimension 3 type-lie though), migrations.rs (post-dec2bd42 — "Restored from 60ca1ad6 — silently reverted by ed697c45" is accurate after `git log` cross-check).

[MINOR] crates/plugin-db/src/audit.rs:5 — "every DDL, validation pass, and (future) backfill writes a row"
  Why: B1 backfill orchestrator (`crate::migrations`) IS shipped — see migrations.rs:1 preamble and the B1 functions `exec_begin`, `exec_commit_batch`, etc. all writing audit rows. The parenthetical "(future)" is stale by ~30 commits.
  Fix: drop "(future)" — backfill is present-tense now.
  Verification: grep -n "future" crates/plugin-db/src/audit.rs
```

### 3. Type lies (wrong return-type or behavioural claims in comments)

```
[CRITICAL] crates/plugin-db/src/orchestrator/register_model/validate.rs:25-30 — Claims SchemaRefused's `to_op_error()` does NOT add `.code`
  Why: The preamble says: "`DbError::SchemaRefused`; that variant's `to_op_error()` arm explicitly does NOT add `.code` to the JS exception (the envelope already carries `"code":"validation_refused"` inside its JSON body)." That is the OPPOSITE of what error.rs:194-205 does. The arm reads:
  ```rust
  DbError::SchemaRefused { code, envelope_json } => {
      OpError::coded(code, envelope_json, None::<String>)
  }
  ```
  It DOES stamp `.code` (the `code` field of the variant is the static "validation_refused" string). This is explicitly verified by error.rs:425-445 (`schema_refused_stamps_code_and_preserves_envelope_as_message` test). The validate.rs preamble inverts the contract.
  Net impact: a reader concludes the SDK has to JSON.parse the message to get the code; in fact `e.code === "validation_refused"` works directly. This is the same misconception that error.rs:9-14 was rewritten to dispel — and the dual-source-of-truth bit. The preamble lies by saying "doesn't add", when in fact it does.
  Fix: Rewrite the paragraph to: "...wraps the envelope in `DbError::SchemaRefused`; that variant's `to_op_error()` arm stamps `.code = "validation_refused"` AND keeps the JSON envelope as the message body, so SDK callers can branch on either `e.code` or `JSON.parse(e.message)` (preserves the documented wire contract while moving fully onto the typed rail)."
  Verification: grep -n "SchemaRefused" crates/plugin-db/src/error.rs (look at to_op_error arm)

[IMPORTANT] crates/plugin-db/src/orchestrator/mod.rs:22 — "Each submodule is `pub(crate)` to scope visibility"
  Why: Look at the literal lines 28-31:
  ```rust
  pub mod auto_tx;
  pub(crate) mod lock_guard;
  pub mod register_model;
  pub mod transaction;
  ```
  Three of four are `pub`, not `pub(crate)`. The effective visibility IS `pub(crate)` because the parent `orchestrator` module is `pub(crate) mod orchestrator;` in lib.rs:84 — but that's transitive, not what the comment says. Either the comment is wrong, or the literal `pub` keywords are wrong (and only `lock_guard` got the right keyword post-cbd12944). Reader gets one mental model from prose and another from code.
  Fix: Either (a) downgrade three `pub mod`s → `pub(crate) mod`, matching the comment + matching what cbd12944 did for lock_guard; or (b) rewrite the comment to "Each submodule is `pub` to the parent module, which is itself `pub(crate)`-gated in `lib.rs`".
  Verification: grep -n "^pub.*mod" crates/plugin-db/src/orchestrator/mod.rs

[MINOR] crates/plugin-db/src/migrations.rs:78 — Stray "duplicate doc summary" line mid-block
  Why: The doc block at lines 67-81 contains TWO summary sentences:
  ```
  /// SQL-error helper — classify the Postgres error through `DbError`
  /// ...
  /// the SQLSTATE classification.
  /// Stamp a `DbError` with a context phrase and convert to `OpError`.
  /// Replaces the previous `coded_sql(context, compio_postgres::Error)`...
  ```
  Line 78 ("Stamp a `DbError` with a context phrase…") reads like a fresh `///` first-paragraph that got pasted into the middle of an already-running doc comment. Renders as one merged paragraph in rustdoc but the prose is jarring. Likely an artifact of dec2bd42's restore.
  Fix: Either drop line 78 (the prior summary at line 67 already covers it) or split into properly separated `///` paragraphs with a blank `///` separator.
  Verification: sed -n '67,81p' crates/plugin-db/src/migrations.rs

[MINOR] crates/plugin-db/src/migrations.rs:79-80 — "Replaces the previous `coded_sql` helper" claim
  Why: The comment says coded_db "Replaces the previous `coded_sql(context, compio_postgres::Error)` helper". But `coded_sql` is alive and well in `crate::audit` (audit.rs:55) with the exact `(context: &str, e: compio_postgres::Error) -> DbError` signature the comment describes as "previous". The narrower truth is: this LOCAL `coded_sql` was replaced (migrations.rs only); the audit-layer one still exists. Slightly misleading.
  Fix: "Replaces the previous `coded_sql(...)` helper in this file" — add "in this file".
  Verification: grep -rn "fn coded_sql" crates/plugin-db/src/

[MINOR] crates/plugin-db/src/migrations.rs:93-96 — Comment claims `message` always starts with "db: "
  Why: The comment says "`message` already starts with "db: " from `walk_pg_chain`" — true ONLY when the DbError was constructed via `From<compio_postgres::Error>` / `DbError::from_pg` (where `walk_pg_chain` is the source). A `DbError::Internal { message: "<bare string>" }` constructed elsewhere (e.g. via `DbError::internal(...)`) won't. The match arm in coded_db unconditionally collapses to `{context}: {message}` — which is correct in practice because every site in this file flowed through `from_pg`, but the comment overgeneralises about the invariant.
  Fix: "the SQL paths in this file produce `message` strings already prefixed by `walk_pg_chain` ("db: ..."); we prepend only the lifecycle context so the output reads `<context>: db: <pg-error>`" — be explicit about the SQL-path qualifier.
  Verification: grep -n "DbError::from_pg\|walk_pg_chain" crates/plugin-db/src/error.rs

[OK] crates/plugin-db/src/error.rs:9-19 — Post-e37b188f preamble
  Why: The "lone hold-out" claim is gone; the new paragraph names the remaining sites (replication.rs / auth/* / diff.rs / validate.rs) by file with rough counts. Cross-checked against actual:
    - replication.rs: 8 sites (claim "~7" — close enough)
    - diff.rs: 3 sites (matches "parts of diff.rs")
    - auth/* total: 22 sites (claim "~15" — undercount by 50%)
  The auth/* number is the most off, but the preamble's purpose is "give the reader an order-of-magnitude" not a precise count, and the "mechanical sweep is pending" framing is honest. Acceptable for now; would suggest "(~20 sites across `auth/{bootstrap,keys,session}.rs`)" on the next pass.
  Verification: grep -c "Result<.*, String>" crates/plugin-db/src/auth/*.rs crates/plugin-db/src/replication.rs crates/plugin-db/src/diff.rs
```

### 4. Missing preambles

```
[OK] Every file in crates/plugin-db/src/ (recursive) has a top-of-file `//!` summary. Verified across:
  - 16 top-level .rs files (audit, broker, context, crud, diff, error, exec, lib, migrations, query, read_set, replication, replication_ops, v8_bridge, wal_consumer, ... plus subdir mods)
  - 4 files in orchestrator/ (auto_tx, lock_guard, mod, transaction)
  - 5 files in orchestrator/register_model/ (apply, bootstrap, mod, plan, validate)
  - 7 files in v8_classes/ (collection, db, migration, migrations, mod, replication, subscription, transaction)
  - 4 files in auth/ (bootstrap, keys, mod, session)
  - 2 files in backend/ (mod, postgres)

  No empty preambles, no `//` (non-doc) leading the file in place of `//!`.
  Verification: head -1 crates/plugin-db/src/**/*.rs (or `for f in crates/plugin-db/src/**/*.rs; do echo "== $f =="; head -1 "$f"; done`)
```

### 5. Orphan TODO/FIXME

```
[CRITICAL] crates/plugin-db/src/query.rs:443-446 — "TODO: A1 composite indexes — wire through `schema_meta.indexes` once the SDK builder exists."
  Why: Composite multi-column indexes ARE wired up. The SDK builder exists (`schema(...).index(name, fields)` in sdks/db/src/types.ts:945, 1010, 1023, 1051+; sdks/db/src/collection.ts:321 docstring). The native side handles them via `build_named_indexes` (query.rs:529) which is called from `orchestrator/register_model/bootstrap.rs:174-176`:
  ```rust
  let named_indexes =
      query::build_named_indexes(app_id, collection, indexes).map_err(DbError::from)?;
  declared_indexes.extend(named_indexes);
  ```
  The end-to-end path is live. The only thing not implemented is `schema_meta.indexes` (a different mechanism — the user provides indexes as a separate JSON arg to `registerModel`, not in `schema._meta`). The TODO confuses readers into thinking composite indexes aren't supported at all.
  Fix: Either delete the TODO (composite indexes shipped via `build_named_indexes` + the indexes arg path), or rewrite to be specific: "TODO(A1): `schema._meta.indexes` declaration form (currently indexes are passed as a separate `registerModel(coll, schema, indexes)` arg; folding them under `schema._meta.indexes` is a future ergonomic — see proposal A1.6)."
  Verification: grep -n "build_named_indexes\|schema_meta\|index.*= true" crates/plugin-db/src/query.rs; grep -n ".index(" sdks/db/src/types.ts

[OK] No FIXME / XXX markers found anywhere under crates/plugin-db/src/.
  Verification: grep -rn "FIXME\|XXX" crates/plugin-db/src/ → empty
```

### 6. AGENTS.md task router — path resolution

```
[OK] Every plugin-db-relevant row in AGENTS.md resolves to a live path on HEAD:
  - "**Adding a native primitive** … `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/`" — all four exist
  - "**The DB SDK** (`@zeroship/db`) … `docs/reference/db.md` · `crates/plugin-db/`" — both exist
  - "**ZS deploy contract** … `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/runtime/src/core/init.rs`" — all three exist (dispatcher.ts, runtime-entry.ts, core/init.rs)
  - Bootstrap docs at the brace-expansion level are not stale.
  Verification: ls docs/reference/{db,plugin-system}.md crates/plugin-{db,kv,storage} crates/runtime-macros sdks/bootstrap/src/{dispatcher,runtime-entry}.ts crates/runtime/src/core/init.rs
```

### 7. lock_guard.rs preamble

```
[OK] crates/plugin-db/src/orchestrator/lock_guard.rs:1-49 — Post-cbd12944 preamble

  Documents (in order):
  - The session-scoped `pg_advisory_lock(hashtext('zs_reg:<app>'), hashtext('register_model'))` invariant.
  - The three commits (b4e533e2, 37a0ef76, 3bb41fa1) that previously open-coded the same release pattern at three pipeline stages.
  - **Drop-can't-await trade-off** (lines 25-41): explicitly explains "Drop::drop is sync; the unlock SQL is async" and lists the three exit modes (release / into_held / panic-fallback) — exactly the design rationale a future reader needs.
  - **Cross-scope hand-off** (line 32-35, into_held): documents that "the lock is intentionally still held by the returned client. The next stage owns the release responsibility." Pairs with apply.rs:32-36 ("Takes the OrchestratorLockGuard separately so the lock can be released between passes") and bootstrap.rs:19-22 ("Returns a `RegisterContext` the later stages thread through, plus a separate `OrchestratorLockGuard`…") so the contract is multi-sited but consistent.
  - **Internal representation** (lines 43-49): explains the `Option<PooledClient>` so `release()` / `into_held()` can move out, and how the `released` flag makes Drop idempotent.

  The body comments (lines 102-130 on release(), 132-156 on into_held(), 159-182 on Drop) all reinforce the preamble's contract. The test suite (lines 184-273) explicitly tests the lifecycle-flag transitions the preamble describes.

  This is the highest-quality new preamble landed since r2.
  Verification: read crates/plugin-db/src/orchestrator/lock_guard.rs
```

### 8. error.rs preamble (r2 fix verification)

```
[OK] crates/plugin-db/src/error.rs:1-42 — Post-e37b188f preamble

  Confirms r2's CRITICAL is closed:
  - "lone hold-out" sweeping claim removed.
  - Replaced with a sourced inventory: "replication.rs (~7 sites), the `auth/*` bootstrap helpers (~15 sites), parts of `diff.rs`, plus the `validate` stage … (a documented SDK wire contract)."
  - Names the boundary discipline: "run_pipeline and the orchestrator dispatchers wrap those strings in typed [DbError] variants at the boundary, so the typed-error invariant still holds at every JS-visible surface — but the SDK loses .code discrimination on the wrapped paths".
  - The "## When to use which variant" table (lines 26-41) is fully synced with the variant set in the enum below.

  Subsequent commits to error.rs (8ff1b2de, c83d6a8c) did not regress the preamble — the body changes are in `to_op_error` arms and behaviour, with the preamble copy untouched.

  Verification: git log -p --follow crates/plugin-db/src/error.rs | head -200
```

### 8b. db.md path fix (r2 fix verification)

```
[OK] docs/reference/db.md:90-97 — Post-e37b188f path fix

  The CRITICAL r2 finding ("crates/runtime/src/bootstrap/db_init.js" — path doesn't exist) is closed. Current text:
  > "The runtime's bootstrap (`sdks/bootstrap/src/runtime-entry.ts`, embedded into the runtime crate at compile time via `crates/runtime/src/core/init.rs::DB_INIT_JS`)"

  Both files exist and the `DB_INIT_JS` const is defined at crates/runtime/src/core/init.rs:273 (verified via grep). The historical context (Stage 5c "dropped manifest-injected schema path") is preserved as a one-liner.

  Verification: ls sdks/bootstrap/src/runtime-entry.ts crates/runtime/src/core/init.rs; grep -n DB_INIT_JS crates/runtime/src/core/init.rs
```

---

## Out-of-scope but flagged for adjacent-doc owners

```
[IMPORTANT] docs/reference/plugin-system.md:315-344 — "Crate structure" tree is 2025-vintage
  Why: This is the canonical "adding a native primitive" entry point (per AGENTS.md row 3). Reader following AGENTS.md → plugin-system.md is greeted by a tree that lists non-existent files and crates:
    - crates/runtime/src/init.rs           → does not exist (today: crates/runtime/src/core/init.rs)
    - crates/runtime/src/runtime.rs        → does not exist
    - crates/runtime/src/plugin.rs         → does not exist (today: crates/runtime/src/base/plugin.rs or similar)
    - crates/plugin-db/src/callbacks.rs    → does not exist (deleted Stage 8b)
    - crates/plugin-db/src/validate.rs     → does not exist (validation lives in query.rs + orchestrator/register_model/validate.rs)
    - crates/plugin-db/src/migrate.rs      → does not exist (today: migrations.rs + audit.rs)
    - crates/plugin-auth/                  → crate does not exist (auth lives inside plugin-db/src/auth/)
    - crates/pg/                           → renamed to crates/compio-postgres/ (Phase 6)
  Outside this audit's primary scope (the task said plugin-db, not plugin-system), but this is the file an "adding a native primitive" reader hits first per AGENTS.md, so the drift matters.
  Fix: Replace the tree with `find crates/plugin-* -name "*.rs" -maxdepth 3` output + the runtime layout from `ls crates/runtime/src/`. Re-pivot from "what files exist" to "what the responsibilities are".
  Verification: ls crates/runtime/src/init.rs crates/plugin-db/src/callbacks.rs crates/plugin-auth/ crates/pg/ → all ENOENT
```

---

## Score: 80 / 100  (r2: 74)

**Delta breakdown (+6 from r2):**

- +5 — Two r2 CRITICALs closed (error.rs lone-holdout claim; db.md broken path). Both fixes are accurate and lint-clean.
- +3 — `lock_guard.rs` preamble is the cleanest new doc landed since r1: covers Drop-can't-await trade-off, hand-off invariant, three exit modes, and Option<Client> representation. Mirrors apply.rs / bootstrap.rs / orchestrator/mod.rs contracts.
- +2 — `migrations.rs` regression-recovery commit (dec2bd42) added inline comments that correctly attribute the silently-reverted lines (line 96 "Restored from 60ca1ad6 — silently reverted by ed697c45"; line 264-266 same shape for tx_connect_failed) — exemplary git-archaeology in code.
- -1 — Stray "duplicate doc summary" at migrations.rs:78 is the only blemish from dec2bd42.
- -2 — `v8_classes/transaction.rs` carries ~12 stale `TX_CONN`/`TX_TOKEN` mentions including 4 rustdoc intra-doc-link broken references — d53f90b0's sweep didn't reach the v8_classes layer and these will emit `cargo doc` warnings.
- -1 — `validate.rs` preamble has a NEW type-lie (claims SchemaRefused.to_op_error doesn't add .code; it does, per error.rs:196-205 and the test at error.rs:425). This is the most consequential new finding because it actively inverts the SDK error-handling contract.
- -1 — `query.rs:443` orphan TODO for composite indexes (already implemented via `build_named_indexes` + the indexes arg path).
- ±0 — `orchestrator/mod.rs:22` literal-vs-effective `pub(crate)` mismatch and lib.rs:110/216 lingering caps-name conventions are minor but pre-existed r2.

**To break 90 next round:**
1. Fix the validate.rs preamble type-lie (CRITICAL — actively misleading).
2. Resolve `v8_classes/transaction.rs` TX_CONN/TX_TOKEN drift (one file, ~12 sites, all mechanical).
3. Resolve query.rs:443 TODO (delete or specify).
4. Decide on `orchestrator/mod.rs` `pub` vs `pub(crate)` and align prose to the choice.
5. Spread the "formerly the X thread-local" footnote pattern to crud.rs:53, exec.rs:329, backend/mod.rs:66, lib.rs:110, lib.rs:216, v8_classes/migration.rs:216 — OR mechanically rename the in-comment occurrences. Either is fine; the inconsistency is what costs points.
