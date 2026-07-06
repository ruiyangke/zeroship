/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/op-ir.schema.json (the engine's single-source-of-
// truth IR schema). Regenerate with: pnpm --filter @zeroship/migrate gen:ir-types
//
// Covers the CLOSED STRING-ENUM IR defs only; the recursive structural types live
// (hand-authored) in ./ir.ts. These are ERGONOMICS; the golden .ir.json corpus is
// the contract (§4.3 / PR3).

/**
 * A binary operator admitted in the closed AST (§3.3.1 method↔node table).
 *
 * Camel/lower-cased on the wire so the JS builder emits the same tokens
 * (`{"node":"binOp","op":"eq", …}`). The set is closed: comparison, boolean,
 * arithmetic, and string concatenation (`||`, the one place PG/SQLite NULL
 * semantics agree — §3.3.1).
 */
export type BinaryOp =
  | "eq"
  | "ne"
  | "lt"
  | "le"
  | "gt"
  | "ge"
  | "and"
  | "or"
  | "add"
  | "sub"
  | "mul"
  | "div"
  | "concat";

/**
 * A unary operator admitted in the closed AST (§3.3.1).
 */
export type UnaryOp = "not" | "isNull" | "isNotNull" | "isTrue" | "isFalse";

/**
 * The allow-listed *named* scalar functions (`c.fn.*` that are NOT engine-
 * synthesized `FnSynth`). CLOSED — a function outside this set has no builder
 * method and no AST variant (§3.3.1.1(a)). These are the provably-identical
 * cross-dialect scalars.
 */
export type ScalarFn =
  | "coalesce"
  | "nullif"
  | "lower"
  | "upper"
  | "trim"
  | "length"
  | "abs"
  | "mod"
  | "round"
  | "floor"
  | "ceil"
  | "substr"
  | "replace"
  | "currentSetting"
  | "currentUser";

/**
 * The engine-SYNTHESIZED helpers (`FnSynth`) whose per-dialect lowering the
 * engine pins (§9). CLOSED. `splitPart` is admitted only within its pinned
 * single-ASCII-delimiter + positive-literal-`n` envelope (validated structurally
 * — §3.3.1.1(b)); `concatWs` is the NULL-skipping join; `now`/`genRandomUuid`
 * are apply-time DB-evaluated scalars (the structured replacement for a frozen
 * `Date.now()` / UUID literal, §4.3).
 */
export type SynthFn = "concatWs" | "splitPart" | "now" | "genRandomUuid";

/**
 * Empty container defaults admitted as column DEFAULTs. This is intentionally
 * EMPTY-only: the IR carries the container kind, not arbitrary JSON/array data.
 */
export type EmptyContainerKind = "object" | "array";

/**
 * The closed cast-target set (§3.3.1), aligned to scalar `ColType` tokens. A
 * non-portable cast target is rejected (`UNSUPPORTED { kind: "expr" }`).
 */
export type CastTarget = "text" | "int" | "real" | "boolean" | "bytes" | "uuid";

/**
 * CLOSED portable field set for SQL `EXTRACT(<field> FROM <expr>)`.
 *
 * Each admitted field has a live three-dialect proof and a faithful renderer on
 * PostgreSQL, SQLite, and MySQL. Fields with PostgreSQL-only semantics live in
 * [`PgExtractField`] instead.
 */
export type ExtractField = "year" | "month" | "day" | "hour" | "minute" | "dow";

/**
 * CLOSED PostgreSQL-only field set for `EXTRACT(<field> FROM <expr>)`.
 *
 * These fields either have no portable SQLite/MySQL analogue or have semantics
 * that diverge under the mandated portable renderers.
 */
export type PgExtractField =
  | "second"
  | "doy"
  | "epoch"
  | "quarter"
  | "week"
  | "isodow"
  | "isoyear"
  | "century"
  | "decade"
  | "millennium"
  | "microseconds"
  | "milliseconds"
  | "timezone"
  | "timezone_hour"
  | "timezone_minute";

/**
 * The CLOSED set of PORTABLE aggregate functions (`c.agg.*`, design §3.4/§3.6).
 *
 * `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` are byte-identical standard SQL on PostgreSQL,
 * SQLite, and MySQL (only the surrounding identifier quoting differs), so there
 * is NO dialect gate — an [`Expr::Agg`] validates and renders on all three.
 */
export type AggFunc = "count" | "sum" | "avg" | "min" | "max";

/**
 * CLOSED per-column index sort-order set. Omitted means the SQL default
 * (`ASC`); renderers spell only `DESC` so default ASC stays byte-identical to
 * the pre-order SQL.
 */
export type IndexSortOrder = "asc" | "desc";

/**
 * The CLOSED index-method lexicon (§3.3.1 `createIndex` `using` union, design
 * line 648). A CLOSED enum — serde rejects any out-of-set token at DESERIALIZE,
 * so a hand-crafted `.ir.json` cannot smuggle an arbitrary / injection-shaped
 * method string into an unvalidated position that would reach the render seam.
 * `gin`/`gist`/`ivfflat`/`hnsw` are Postgres-only logical hints; `fts5` maps to
 * the SQLite FTS5 virtual-table path (per-dialect lowering is Wave C's job).
 * Camel/lower-cased on the wire (`"btree"`, `"ivfflat"`, …).
 */
export type IndexMethod = "btree" | "brin" | "gin" | "gist" | "ivfflat" | "hnsw" | "fts5";

/**
 * A comparison operator for a [`Precondition::RowCount`] assertion.
 */
export type CmpOp = "Eq" | "Ne" | "Lt" | "Le" | "Gt" | "Ge";

/**
 * What to do when a precondition is **unmet** (evaluates false).
 */
export type OnUnmet = "Halt" | "Skip";

/**
 * The phase of a zero-downtime **expand-contract** online migration (design
 * §5, Plan 8). Carried only by `online` migrations (`flags.online == true`);
 * `None` for an ordinary one-shot migration.
 *
 * An online column RENAME (or type change) is split across **two deploys**:
 *
 * - **`Expand`** — additively grow the schema so old and new shapes coexist
 *   (add the new nullable column, install a dual-write trigger, backfill).
 *   Lands *before* dependent code switches over.
 * - **`Contract`** — drop the old shape once no code uses it (drop the
 *   trigger + function, drop the old column). Lands *after* code switches over.
 *
 * The engine enforces the split via a gate (design Plan 8 v1.2): a `Contract`
 * migration is refused unless every `Expand` migration it `depends_on` is
 * **net-applied in the journal**. This makes the journal the single source of
 * truth for the expand→contract timeline and gives cross-deploy partitioning
 * for free (a separate, later deploy can apply the contract).
 */
export type OnlinePhase = "Expand" | "Contract";

/**
 * **PR10** — the uniform existence-guard modifier (§2.7). Carried on a guarded
 * DDL op as `existence_guard: Option<ExistenceGuard>` (omitted-when-absent on
 * the wire). The engine SYNTHESIZES the guard via an executor-side CATALOG PROBE
 * (decide-in-Rust: probe → run-or-skip), NEVER by lowering to a native
 * `IF [NOT] EXISTS` clause — native support is patchy and asymmetric across PG /
 * SQLite (PG has no `ADD CONSTRAINT IF NOT EXISTS` / none on alter/rename;
 * SQLite has no `ADD COLUMN IF NOT EXISTS` / none on drop-column/rename). A
 * CLOSED 2-variant enum so serde rejects any other token at deserialize and the
 * validate-time legal-direction check (`ifNotExists` on create* /add*; `ifExists`
 * on drop* /rename/alter) is a total match. Camel-cased on the wire
 * (`"ifNotExists"`, `"ifExists"`).
 */
export type ExistenceGuard = "ifNotExists" | "ifExists";

/**
 * The CLOSED referential-action lexicon for a FOREIGN KEY's `ON DELETE` /
 * `ON UPDATE` clause (C1 — design §3.3). A CLOSED enum so the schema enumerates
 * exactly the supported actions and serde REJECTS any out-of-set token at
 * DESERIALIZE — a hand-crafted `.ir.json` cannot smuggle an arbitrary /
 * injection-shaped action string into the FK render seam. Camel-cased on the
 * wire (`"cascade"`, `"setNull"`, `"noAction"`, …); the per-dialect SQL spelling
 * (`SET NULL`, `NO ACTION`, …) is the render seam's job via
 * [`zeroship_schema::query::normalize_fk_action`].
 */
export type RefAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction";

/**
 * CLOSED exclusion access-method set. PostgreSQL supports more methods, but the
 * IR only admits the audited methods below.
 */
export type ExclusionMethod = "gist" | "spgist" | "btree";

/**
 * CLOSED exclusion-operator set. The SQL operator spelling is rendered from
 * this enum, never carried as an arbitrary string.
 */
export type ExclusionOperator = "&&" | "=" | "<>" | "<" | ">" | "<=" | ">=";

/**
 * The CLOSED trigger-timing lexicon (`BEFORE`/`AFTER`/`INSTEAD OF`).
 */
export type TriggerTiming = "before" | "after" | "insteadOf";

/**
 * The CLOSED trigger-event lexicon (`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE`),
 * joined by `OR` in `CREATE TRIGGER … BEFORE UPDATE OR DELETE`. `TRUNCATE`
 * renders on Postgres and is refused on SQLite as a per-facet unsupported shape.
 */
export type TriggerEvent = "insert" | "update" | "delete" | "truncate";

/**
 * The CLOSED trigger `FOR EACH {ROW|STATEMENT}` lexicon. `STATEMENT` renders on
 * Postgres and is refused on SQLite as a per-facet unsupported shape.
 */
export type ForEach = "row" | "statement";

/**
 * The closed trigger-body raise levels.
 */
export type RaiseLevel = "abort" | "fail" | "ignore" | "rollback";

/**
 * A closed join kind in the SELECT subset.
 */
export type JoinKind = "inner" | "left";

/**
 * Direction for `ORDER BY`.
 */
export type OrderDir = "asc" | "desc";

/**
 * **VENDOR (`@zeroship/migrate/pg`)** — the CLOSED privilege lexicon for
 * `Op::Grant`/`Op::Revoke` (vendor spec §2.3). A CLOSED enum, so serde REJECTS an
 * out-of-set token at DESERIALIZE — a hand-crafted `.ir.json` cannot smuggle an
 * injection-shaped privilege string into the GRANT render seam (the
 * `RefAction`/`IndexMethod` precedent). `All` renders `ALL PRIVILEGES`; the rest
 * render their SQL keyword. Camel/lower-cased on the wire.
 */
export type Privilege =
  | "all"
  | "select"
  | "insert"
  | "update"
  | "delete"
  | "truncate"
  | "references"
  | "trigger"
  | "usage"
  | "connect"
  | "create"
  | "execute"
  | "temporary";

/**
 * **VENDOR** — the CLOSED `CREATE POLICY … FOR <cmd>` lexicon.
 */
export type PolicyCmd = "all" | "select" | "insert" | "update" | "delete";

/**
 * **VENDOR** — the CLOSED function-argument mode lexicon.
 */
export type FuncArgMode = "in" | "out" | "inout";

/**
 * **VENDOR** — the CLOSED `CREATE FUNCTION … LANGUAGE` lexicon. A deliberately
 * 2-set: `plpgsql`/`sql` ONLY — an untrusted PL (`plpythonu`/`plperlu`/`c`) is
 * REJECTED at DESERIALIZE (serde unknown-variant) BEFORE the body deny-list scan
 * even runs (vendor spec §2.6 / §3.3).
 */
export type FuncLanguage = "plpgsql" | "sql";

/**
 * **VENDOR** — the CLOSED function-volatility lexicon.
 */
export type FuncVolatility = "volatile" | "stable" | "immutable";

/**
 * Per-collection deploy-time data-validation strictness, mirroring the
 * `@zeroship/db` `schema(...).strictness(...)` builder.
 */
export type TableStrictness = "strict" | "lenient" | "off";
