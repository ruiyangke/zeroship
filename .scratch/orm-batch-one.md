# ORM: batch one (work order)

Base: main at `970093576`. The plan file `.scratch/wire-the-query-ir.md` no longer exists; this
supersedes it. Rebuild anything else from `docs/proposals/2026-08-26-sc3-dbplan-ir-and-ledger.md`.

## 1. What the re-validation changed

- DEAD, do not build: plan commit 7 (widen `IdentRole::Namespace`) and anything downstream of it -
  a `SchemaName`-to-`Ident` bridge, a hyphen-tolerant namespace, a second `IdentRole`. CONFIRMED:
  no production path constructs a namespace; every `IdentRole::Namespace` site is a test.
- DEAD: a `MysqlValueFormat` (`SqlDialect::Mysql` is render-only), and a hand-classified ledger
  file. CONFIRMED.
- DONE: plan commits 1 (`49e743269`, completed by `ca77c8059`) and 11 (`7dd6ad9b5`). CONFIRMED.
- MOOT: "the flip gates the ORM". CONFIRMED it does not. The flip is not necessary (the schema is a
  derivation, `app_derivation::schema_name`) and not sufficient (it moves all eleven derivations at
  once; `meter_key` and `encryption_salt` fail silently).
- INVERTED dependency: the metering re-key is a prerequisite OF the flip, not of the ORM.
- Corrected sequence: land the six in-crate renderer-seam commits below, then the schema-derivation
  move, then the meter re-key, then the flip. The ORM does not wait on any of them.

## 2. The batch

Scope is `db` throughout (the crate's own history uses it). Six commits, in order. All six are
verifiable; the first five need nothing but `cargo test -p zeroship-data-query-builder`.

### C1 `test(db): pin that a slot's first mention never trails a higher one`

New test target `crates/zeroship-data-query-builder/tests/placeholder_order.rs`. Over every family
(`render_select`, `render_insert`, `render_update`, `render_delete`, `render_search`), walk
`RenderedSql::sql()` left to right, record each slot at its first textual occurrence, and assert the
resulting sequence is `1, 2, ..., N`. This is the arm that records why the SQLite renderer must
spell `?n`: SQLite indexes a `$NAME` parameter by first textual appearance and ignores its digits,
and `crates/zeroship-data-sqlite/src/session.rs` binds positionally, so a non-monotonic statement
mis-binds with no error. CONFIRMED (measured on sqlite3 3.51.2: with `$1='ONE'`, `$2='TWO'`,
`SELECT $2, $1, ?1` returns `TWO|ONE|TWO`).

- Verify: `cargo test -p zeroship-data-query-builder --test placeholder_order`
- SAFE. No live database. No `pnpm build`.

### C2 `refactor(db): thread the value format through the writer instead of naming postgres`

`Writer` and the private writer functions in `crates/zeroship-data-query-builder/src/render/postgres.rs`
move to `render/mod.rs`; `Writer` gains a `&dyn ValueFormat` field. The three sites that name
`PostgresValueFormat` concretely (`Writer::write_bound` via `placeholder_for`, the
`Assignment::CurrentTimestamp` arm of `write_assignments`, and `write_bounded_target`) become
`self.format`. CONFIRMED those are exactly three sites. No SQL changes, so the byte-exact fixtures in
`tests/determinism.rs`, `tests/write_family.rs` and `tests/search_family.rs` are the regression test.

- Verify: `cargo test -p zeroship-data-query-builder`
- SAFE. No live database.

### C3 `refactor(db)!: make the bounded-write identity a validated ident, not a dialect spelling`

Delete `ValueFormat::row_identity_column`. Add a crate-private `row_identity() -> Ident` in
`render/mod.rs` that parses `"id"` under `IdentRole::Column`; `write_bounded_target` routes it
through `quote`. `quote_raw` then has no caller but `quote` and folds into it, making the module's
claim ("no public function accepts a `&str` that reaches statement text") true of the private surface
too. CONFIRMED the method is platform policy, not a dialect spelling: `zeroship-schema`'s `query.rs`
injects `id TEXT PRIMARY KEY` on both tiers, while the shipped `single_row_write_target` answers
`rowid` for SQLite - so leaving the method on the trait invites a future SQLite impl to write the
wrong answer. Keep one `compile_fail` doc arm proving a stale impl is `E0407`.

The commit body must quote and rebut the `# FOR UPDATE is load-bearing, not decoration` block in
`render/postgres.rs`. That block names a filter-invalidation hazard, not lost-update. The rebuttal is
that the identity column is unchanged by this commit and `FOR UPDATE` stays; only the spelling's
provenance moves. PLAUSIBLE that the single-writer argument also answers it, but that argument
belongs to the SQLite renderer, not here.

- Verify: `cargo test -p zeroship-data-query-builder`
- SAFE. No live database. Breaking to the crate's public trait; no consumer exists.

### C4 `feat(db): let rendered sql count placeholders in the scan the dialect wrote them in`

`RenderedSql` carries the scan the writer used, as an enum, NOT a `char` sigil.
`placeholder_count` and `placeholder_slots` dispatch on it. Two arms: `SigilIndexed { sigil }`
(PostgreSQL `$n`, SQLite `?n`) and `Positional` (MySQL's bare `?`, where the slot set is
`1..=count`). Update the doc on both methods: the current text asserts scanning for `$` is sound and
that assertion becomes dialect-scoped. Add the arm that is missing today - handed the wrong scan, the
scanner must be VISIBLY empty (a distinguishable error or `None`), never a quiet zero, because a
quiet zero makes every SC-3 third-invariant assertion in `tests/parameters_never_carry_values.rs`,
`tests/write_family.rs` and `tests/search_family.rs` pass vacuously. CONFIRMED both methods
hard-code `b'$'` today.

- Verify: `cargo test -p zeroship-data-query-builder`
- SAFE. No live database.

### C5 `feat(db): refuse a write whose bind budget is not the rendering dialect's`

`render_insert` (and `render_update` / `render_delete` where a budget is reachable) compares
`plan.budget().dialect_name()` against `ValueFormat::dialect_name()` and returns a `RenderError`
on mismatch. CONFIRMED both accessors exist: `BindBudget::dialect_name` is a `const fn` on the
`&'static str` field, and `Insert::budget` returns the budget by value. CONFIRMED the render module
never mentions `budget` today, so an `Insert` admitted under `BindBudget::POSTGRES` and handed to a
SQLite renderer would produce a statement the driver refuses from a plan the IR blessed. Two refusal
arms plus one matching control.

- Verify: `cargo test -p zeroship-data-query-builder`
- SAFE. No live database.

### C6 `test(db): run the search ir live target in the plugin-db suite`

Add `search_ir_live` to the `for target in ...` list in `tests/run_plugin_db_live_suite.sh` (today:
`integration native_transaction distributed_live missing_role column_grants`). CONFIRMED that list,
and CONFIRMED the target is referenced by no shell script or workflow file in the tree. The suite IS
in CI, so the only arm binding the IR to the shipped builder
(`the_ir_and_the_shipped_builder_rank_identically`) has never run there. The gate-arm floor for that
loop must go up by one in the same commit.

- Verify, cheap: `cargo test -p zeroship-plugin-db --features live-db-tests --test search_ir_live -- --list`
- Verify, real: `bash tests/run_plugin_db_live_suite.sh`
- NEEDS-BUILD, and needs a LIVE DATABASE with pgvector and PostGIS plus
  `deploy/ops/zeroship.test.toml`, which a fresh worktree lacks.

## 3. C1, expanded

One new file, one existing file touched only if the helper is judged to belong in the crate. Prefer
the test-only form: it adds no production surface.

**`crates/zeroship-data-query-builder/tests/placeholder_order.rs` (new).**

- Module doc: state the property (a slot's first mention never trails a higher slot's first
  mention), state why it matters (SQLite assigns `$NAME` indices by first textual appearance and
  ignores the digits; `zeroship-data-sqlite`'s session binds positionally), and record the
  measurement verbatim as a fenced block: `$1='ONE'`, `$2='TWO'`, `SELECT $2, $1, ?1` returns
  `TWO|ONE|TWO`, control `SELECT $1, $2, ?1` returns `ONE|TWO|ONE`. Cite
  `crates/zeroship-data-sqlite/src/session.rs` by path and name the binding call; no line numbers.
- `fn first_mentions(sql: &str) -> Vec<usize>`: scan for `$` followed by an ASCII digit run, parse
  the run, push the slot the first time it is seen. Returns first-appearance order, which is exactly
  what `RenderedSql::placeholder_slots` cannot express (it sorts and dedups).
- `fn assert_monotonic(name: &str, rendered: &RenderedSql) -> usize`: assert
  `first_mentions(rendered.sql()) == (1..=n).collect()` where `n = rendered.params().len()`; return
  `n`. The failure message must print the fixture name and the observed order.
- Five test fns, one per family, each building a plan through the ordinary constructors already used
  in `tests/write_family.rs` and `tests/search_family.rs` (reuse those plan shapes; do not invent
  new ones). Search must include the shape where the distance operand is bound once and written
  twice - that is the only family where occurrence count exceeds slot count, and it is the shape most
  likely to break monotonicity.
- One counter-control, `the_helper_sees_a_violation`: hand a literal string with `$2` before `$1` to
  `first_mentions` and assert it returns `[2, 1]`. Without this the whole file could be scanning
  nothing and reporting green.
- One arm-count guard: every family fixture must return `n >= 1` from `assert_monotonic`, and the
  file asserts the total number of fixtures ruled on clears a floor declared beside them. A fixture
  that renders zero placeholders is trivially monotonic and must not be allowed to count.

**No production file changes.** If review prefers the scan to live in the crate, it goes on
`RenderedSql` as `placeholder_first_mentions`, and then C4 must dispatch it on the scan kind too -
which is a reason to keep it in the test until C4 has landed.

## 4. Still blocked

- **The `apps.id` column flip** - blocked on the metering re-key (`app_derivation::meter_key` feeds a
  map `zeroship_metering::meter::Meter::drain` parses with `Uuid::parse_str`, dropping the counters on
  `Err`); it gates production dispatch of the IR and nothing else. CONFIRMED.
- **The schema-derivation move** (`apply.rs` and `binding_for_isolate` onto
  `canonical_app_id_for(&app.uuid())`) - blocked on an enumeration with a stated denominator of every
  site composing an app-id-derived name from the shared `APP_ID` env slot rather than from
  `app_derivation`; it gates admitting the IR to production. The four `app_derivation` names as
  outstanding are a floor, not the set. PLAUSIBLE that the schema is separable; NOT measured.
- **D2 (aggregation)** - blocked on an IR node change: `OrderKey` must be able to name an output
  alias or an `AggregateRef`, and `PlanError::UngroupedProjectedField` fires before
  `UngroupedOrderKey` at the same site, so fixing only the order-key arm still refuses the documented
  `$group` + `$sort` pipeline. It gates the aggregation family. CONFIRMED.
- **D3 (`$like` / `$ilike` on SQLite)** - blocked on an owner decision about the dev tier's LIKE
  contract, which is coupled to a connection-scoped `PRAGMA case_sensitive_like` in
  `zeroship-data-sqlite`'s session, so it cannot be settled inside this crate. It gates the SQLite
  renderer's `Pattern` arm, which must refuse `ILike`/`NotILike` until then. CONFIRMED that SQLite's
  `LIKE` is already ASCII case-insensitive and the shipped `COLLATE NOCASE` is inert.
