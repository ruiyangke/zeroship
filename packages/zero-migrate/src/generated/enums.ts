/* eslint-disable */
// GENERATED FILE — do not edit by hand.
// Source: crates/zeroship-migrate/ir-envelope.schema.json (the engine's single-source-of-
// truth IR schema). Regenerate with: pnpm --filter @zeroship/migrate gen:ir-types
//
// Covers the CLOSED STRING-ENUM IR defs only; the recursive structural types live
// (hand-authored) in ./ir.ts. These are ERGONOMICS; the golden IR-envelope corpus is
// the contract.

/**
 * A binary operator admitted in the closed AST (the method-to-node table).
 *
 * Camel/lower-cased on the wire so the JS builder emits the same tokens
 * (`{"node":"binOp","op":"eq", ...}`). The set is closed: comparison, boolean,
 * arithmetic, and string concatenation (`||`, the one place PG/SQLite NULL
 * semantics agree).
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
 * A unary operator admitted in the closed AST.
 */
export type UnaryOp = "not" | "isNull" | "isNotNull" | "isTrue" | "isFalse";

/**
 * The allow-listed *named* scalar functions (`c.fn.*` that are NOT engine-
 * synthesized `FnSynth`). CLOSED - a function outside this set has no builder
 * method and no AST variant. These are the provably-identical
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
 * engine pins. CLOSED. `splitPart` has dialect-neutral literal grammar and a
 * backend-owned portability envelope; `concatWs` is the NULL-skipping join;
 * `now` is an apply-time DB-evaluated scalar (the structured replacement for a
 * frozen `Date.now()` literal).
 */
export type SynthFn = "concatWs" | "splitPart" | "now";

/**
 * Empty container defaults admitted as column DEFAULTs. This is intentionally
 * EMPTY-only: the IR carries the container kind, not arbitrary JSON/array data.
 */
export type EmptyContainerKind = "object" | "array";

/**
 * The closed cast-target set, aligned to scalar `ColType` tokens. A
 * non-portable cast target is rejected (`UNSUPPORTED { kind: "expr" }`).
 */
export type CastTarget = "text" | "int" | "real" | "boolean" | "bytes" | "uuid";

/**
 * The CLOSED field set for SQL `EXTRACT(<field> FROM <expr>)`.
 *
 * ONE set for one SQL construct. This used to be two enums - a six-member
 * `ExtractField` and a fifteen-member `PgExtractField` - split on a claim about
 * which parts are portable. That claim is not core's to make: it is a fact
 * about the shipping backends, it cannot be right for a backend that does not
 * exist yet, and it was already wrong (MySQL renders `QUARTER`, `WEEK` and
 * `MICROSECOND` natively, all three of which sat under the PostgreSQL name).
 *
 * Which parts a target can actually render is asked per field, per backend,
 * through
 * [`ExprDialectFeature::Extract`](crate::validate::ExprDialectFeature::Extract),
 * and spelled by that backend's `DmlRenderer::render_extract`.
 */
export type ExtractField =
  | "year"
  | "month"
  | "day"
  | "hour"
  | "minute"
  | "dow"
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
  | "timezoneHour"
  | "timezoneMinute";

/**
 * The CLOSED set of PORTABLE aggregate functions (`c.agg.*`).
 *
 * `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` are byte-identical standard SQL on `PostgreSQL`,
 * `SQLite`, and `MySQL` (only the surrounding identifier quoting differs), so there
 * is NO dialect gate - an [`Expr::Agg`] validates and renders on all three.
 */
export type AggFunc = "count" | "sum" | "avg" | "min" | "max" | "stringAgg" | "arrayAgg" | "boolAnd" | "boolOr";

/**
 * CLOSED per-column index sort-order set. Omitted means the SQL default
 * (`ASC`); renderers spell only `DESC` so default ASC stays byte-identical to
 * the pre-order SQL.
 */
export type IndexSortOrder = "asc" | "desc";

/**
 * The CLOSED index-method lexicon (`createIndex` `using` union). A CLOSED enum - serde rejects any out-of-set token at DESERIALIZE,
 * so a hand-crafted IR envelope cannot smuggle an arbitrary / injection-shaped
 * method string into an unvalidated position that would reach the render seam.
 * `gin`/`gist`/`ivfflat`/`hnsw` are Postgres-only logical hints (per-dialect
 * lowering is the render seam's job).
 * Camel/lower-cased on the wire (`"btree"`, `"ivfflat"`, ...).
 */
export type IndexMethod = "btree" | "brin" | "gin" | "gist" | "ivfflat" | "hnsw";

/**
 * A comparison operator for a [`Precondition::RowCount`] assertion.
 */
export type CmpOp = "Eq" | "Ne" | "Lt" | "Le" | "Gt" | "Ge";

/**
 * What to do when a precondition is **unmet** (evaluates false).
 */
export type OnUnmet = "Halt" | "Skip";

/**
 * The phase of a zero-downtime **expand-contract** online migration.
 * Carried only by `online` migrations (`flags.online == true`);
 * `None` for an ordinary one-shot migration.
 *
 * An online column RENAME (or type change) is split across **two deploys**:
 *
 * - **`Expand`** - additively grow the schema so old and new shapes coexist
 *   (add the new nullable column, install a dual-write trigger, backfill).
 *   Lands *before* dependent code switches over.
 * - **`Contract`** - drop the old shape once no code uses it (drop the
 *   trigger + function, drop the old column). Lands *after* code switches over.
 *
 * The engine enforces the split via a gate: a `Contract`
 * migration is refused unless every `Expand` migration it `depends_on` is
 * **net-applied in the journal**. This makes the journal the single source of
 * truth for the expand->contract timeline and gives cross-deploy partitioning
 * for free (a separate, later deploy can apply the contract).
 */
export type OnlinePhase = "Expand" | "Contract";

/**
 * the uniform existence-guard modifier. Carried on a guarded
 * DDL op as `existence_guard: Option<ExistenceGuard>` (omitted-when-absent on
 * the wire). The engine SYNTHESIZES the guard via an executor-side CATALOG PROBE
 * (decide-in-Rust: probe -> run-or-skip), NEVER by lowering to a native
 * `IF [NOT] EXISTS` clause - native support is patchy and asymmetric across PG /
 * `SQLite` (PG has no `ADD CONSTRAINT IF NOT EXISTS` / none on alter/rename;
 * `SQLite` has no `ADD COLUMN IF NOT EXISTS` / none on drop-column/rename). A
 * CLOSED 2-variant enum so serde rejects any other token at deserialize and the
 * validate-time legal-direction check (`ifNotExists` on create* /add*; `ifExists`
 * on drop* /rename/alter) is a total match. Camel-cased on the wire
 * (`"ifNotExists"`, `"ifExists"`).
 */
export type ExistenceGuard = "ifNotExists" | "ifExists";

/**
 * The CLOSED referential-action lexicon for a FOREIGN KEY's `ON DELETE` /
 * `ON UPDATE` clause. A CLOSED enum so the schema enumerates
 * exactly the supported actions and serde REJECTS any out-of-set token at
 * DESERIALIZE - a hand-crafted IR envelope cannot smuggle an arbitrary /
 * injection-shaped action string into the FK render seam. Camel-cased on the
 * wire (`"cascade"`, `"setNull"`, `"noAction"`, ...); the per-dialect SQL spelling
 * (`SET NULL`, `NO ACTION`, ...) is the render seam's job via
 * `zeroship_migrate::schema::query::normalize_fk_action`.
 */
export type RefAction = "cascade" | "restrict" | "setNull" | "setDefault" | "noAction";

/**
 * CLOSED exclusion access-method set. `PostgreSQL` supports more methods, but the
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
 * joined by `OR` in `CREATE TRIGGER ... BEFORE UPDATE OR DELETE`. `TRUNCATE`
 * renders on Postgres and is refused on `SQLite` as a per-facet unsupported shape.
 */
export type TriggerEvent = "insert" | "update" | "delete" | "truncate";

/**
 * The CLOSED trigger `FOR EACH {ROW|STATEMENT}` lexicon. `STATEMENT` renders on
 * Postgres and is refused on `SQLite` as a per-facet unsupported shape.
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
 * **VENDOR (`@zeroship/migrate`)** - the CLOSED privilege lexicon for
 * `Op::Grant`/`Op::Revoke`. A CLOSED enum, so serde REJECTS an
 * out-of-set token at DESERIALIZE - a hand-crafted IR envelope cannot smuggle an
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
 * **VENDOR** - the CLOSED `CREATE POLICY ... FOR <cmd>` lexicon.
 */
export type PolicyCmd = "all" | "select" | "insert" | "update" | "delete";

/**
 * **VENDOR** - the CLOSED function-argument mode lexicon.
 */
export type FuncArgMode = "in" | "out" | "inout";

/**
 * **VENDOR** - the CLOSED `CREATE FUNCTION ... LANGUAGE` lexicon. A deliberately
 * 2-set: the plain SQL body language, or the TARGET'S OWN procedural language -
 * nothing else. An externally installed PL (`plpythonu`/`plperlu`/`c`) has no
 * spelling here at all, so it is REJECTED at DESERIALIZE (serde
 * unknown-variant) BEFORE the body deny-list scan even runs.
 *
 * The 2-set is the ENGINE's security decision, not one server's language
 * namespace: a target may trust several installed PLs and this vocabulary still
 * offers exactly two. That is why the set stays closed HERE while the token each
 * member renders to stays with the backend that renders it - the one place that
 * knows what its procedural language is called.
 */
export type FuncLanguage = "procedural" | "sql";

/**
 * **VENDOR** - the CLOSED function-volatility lexicon.
 */
export type FuncVolatility = "volatile" | "stable" | "immutable";

/**
 * Per-collection deploy-time data-validation strictness, mirroring the
 * built-in `schema(...).strictness(...)` builder.
 */
export type TableStrictness = "strict" | "lenient" | "off";
