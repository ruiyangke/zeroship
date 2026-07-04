// `@zeroship/migrate` — the fluent-only op-builder DSL implementation
// (design `2026-06-25-op-dsl-fluent-redesign.md`).
//
// This is the TS authoring surface a creator imports:
//   import { table, t } from "@zeroship/migrate";
//
//   export default {
//     up() {
//       table("users")
//         .column("first_name").add({ type: t.text() })
//         .backfill({ set: { first_name: c => c.fn.splitPart(c("name"), " ", 1) } });
//     },
//   };
//
// It is the typed peer of the engine-embedded recorder
// (`crates/zeroship-migrate/src/frontend/migrate_ops.js`, which the Rust runtime
// `include_str!`s into V8 at build/record time). Both emit the IDENTICAL
// dialect-neutral op objects the closed Rust `Op` enum / `op-ir.schema.json`
// deserialize — the `.ir.json` wire shape is frozen (byte-identical to the
// pre-redesign flat surface except the C1 FK-actions delta), and the golden
// corpus (`tests/op_fixtures`) + the `Checksum::of_ir` round-trip are the
// contract.
//
// `table()` is the reusable table DDL/DML entry. The flat op-functions are GONE
// from the public API; their op-construction logic survives as the internal
// `recordX` helpers the handle delegates to (so the IR is unchanged). Every terminal
// RECORDS one canonical op object onto the ambient per-migration recorder
// synchronously, returning the handle. Recording OUTSIDE an active recorder
// throws a structured `OP_OUTSIDE_RECORDER`.

import type {
  BackfillArgs,
  CheckDef,
  CheckRef,
  ColType,
  ColumnDef as ColumnDefType,
  ColumnRef,
  ConstraintRef,
  CommentTargetArg,
  CreateRawViewArgs,
  AlterSequenceArgs,
  CreateDomainArgs,
  CreateEnumArgs,
  CreateSequenceArgs,
  CreateTablePolicyArgs,
  CreateTriggerArgs,
  CreateTableArgs,
  CreateViewArgs,
  DbSynthSymbol,
  DefaultValue,
  DelArgs,
  DomainHandle,
  DeterminismFinding,
  DropTablePolicyArgs,
  DropDomainArgs,
  DropEnumArgs,
  DropSequenceArgs,
  DropViewArgs,
  ExclusionAddArgs,
  ExclusionConstraintArgs,
  ExclusionElementArg,
  ExclusionRef,
  Expr,
  ExprBuilder,
  ExprChain as ExprChainType,
  ExprFn,
  EnumHandle,
  FnNamespace,
  ForeignKeyRef,
  ForeignKeyReference,
  GeneratedOptions,
  IdOptions,
  IdentityOptions,
  IndexRef,
  IndexElementArg,
  InsertArgs,
  IndexStorageParamsArg,
  Join,
  JoinKind,
  MaskOptions,
  OrderItem,
  PartitionBoundInput,
  PartitionBoundSentinel,
  PartitionBuilder,
  PartitionHandle,
  PartitionOptions,
  PartitionSpec,
  PgExprNamespace,
  RefAction,
  Row,
  ScalarValue,
  SelectAst,
  SelectItem,
  SequenceHandle,
  TableHandle,
  TableOptions,
  TableStrictness,
  TableRef,
  TriggerBodyBuilder,
  TriggerStmt,
  TypeLexicon,
  UniqueRef,
  UpdateArgs,
  VectorOptions,
  ViewHandle,
  ViewOptions,
  ViewQueryBuilder,
} from "./types.js";

import { TypeBuilder as DbTypeBuilder } from "@zeroship/db";

import { colTypeFromDbField, type DbSchemaField } from "./db-lexicon.js";

import type { Classification, MaskKind, VectorMetric } from "./generated/ir.js";

type Node = Record<string, unknown>;

// ── The ambient recorder (§3.1 / §5) ──

const nativeDateNow = typeof Date !== "undefined" ? Date.now : undefined;
const nativeMathRandom = typeof Math !== "undefined" ? Math.random : undefined;
const nativeCryptoRandomUUID =
  typeof globalThis.crypto !== "undefined" && typeof globalThis.crypto.randomUUID === "function"
    ? globalThis.crypto.randomUUID
    : undefined;

function nativeFnSynthName(value: unknown): "now" | "genRandomUuid" | undefined {
  if (value === nativeDateNow) return "now";
  if (value === nativeMathRandom) return "genRandomUuid";
  if (nativeCryptoRandomUUID !== undefined && value === nativeCryptoRandomUUID) {
    return "genRandomUuid";
  }
  return undefined;
}

function nativeFnSynthNode(value: unknown): Node | undefined {
  const fn = nativeFnSynthName(value);
  return fn === undefined ? undefined : { node: "fnSynth", fn, args: [] };
}

const INVALID_FUNCTION_VALUE_MESSAGE =
  "function values are not valid here; only the supported native symbols " +
  "Date.now / Math.random / crypto.randomUUID translate to DB-evaluated scalars";

function rejectFunctionValue(value: unknown): void {
  if (typeof value === "function") {
    throw structuredError("OP_INVALID", INVALID_FUNCTION_VALUE_MESSAGE);
  }
}

/** A handed-out, not-yet-terminated selector the recorder tracks (§5). */
interface PendingSelector {
  selector: string;
  name: string;
  terminated: boolean;
}

interface Recorder {
  ops: Node[];
  /** Every selector the handle handed out this phase, keyed by a monotonic id. */
  pending: Map<number, PendingSelector>;
  nextSelectorId: number;
}

type RecorderPhase = "up" | "down";

let active: Recorder | null = null;
const deferredUpOps: Node[] = [];

function structuredError(code: string, message: string, extra?: Record<string, unknown>): Error {
  const err = new Error(message) as Error & Record<string, unknown>;
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

/** Begin a fresh recording buffer (the build evaluator calls this before a phase). */
export function __begin(phase: RecorderPhase = "up"): void {
  active = {
    ops: phase === "up" ? deferredUpOps.map((op) => structuredClone(op)) : [],
    pending: new Map(),
    nextSelectorId: 0,
  };
}

/**
 * Drain + return the recorded op list, clearing the active recorder. At DRAIN
 * (not eagerly — so a var-held selector terminated on a later line is fine, §5),
 * any selector that was handed out but never terminated is a hard structured
 * `SELECTOR_NOT_TERMINATED` error.
 */
export function __drain(): Node[] {
  if (active === null) return [];
  const rec = active;
  active = null;
  for (const sel of rec.pending.values()) {
    if (!sel.terminated) {
      throw structuredError(
        "SELECTOR_NOT_TERMINATED",
        `selector .${sel.selector}(${JSON.stringify(sel.name)}) was never terminated; ` +
          "a selector records nothing until one of its terminals is called",
        {
          selector: sel.selector,
          name: sel.name,
          suggested_fix: `call a terminal on .${sel.selector}(${JSON.stringify(sel.name)}) ` +
            "(e.g. .add({…})) or remove the selector",
        },
      );
    }
  }
  return rec.ops;
}

function recorder(): Recorder {
  if (active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      "op authoring called outside an active migration recorder; " +
        "the table() handle may only be used synchronously inside up()/down()",
      { suggested_fix: "move the table()/selector calls inside the migration's up()/down() body" },
    );
  }
  return active;
}

function push(op: Node): Node {
  recorder().ops.push(op);
  return op;
}

function pushOrDeferUp(op: Node): Node {
  if (active === null) {
    deferredUpOps.push(op);
    return op;
  }
  active.ops.push(op);
  return op;
}

/** Internal hook used only by the `@zeroship/migrate/pg` subpath. */
export function __pgPush(op: Node): Node {
  return push(op);
}

/** Register a handed-out selector; returns its id (used at terminate). */
function registerSelector(selector: string, name: string): number {
  const rec = recorder();
  const id = rec.nextSelectorId++;
  rec.pending.set(id, { selector, name, terminated: false });
  return id;
}

/** Mark a selector terminated; double-terminate is a structured error (§5). */
function terminateSelector(id: number): void {
  const rec = recorder();
  const sel = rec.pending.get(id);
  // `sel` is always present for a live id; defensive only.
  if (sel === undefined) return;
  if (sel.terminated) {
    throw structuredError(
      "SELECTOR_ALREADY_TERMINATED",
      `selector .${sel.selector}(${JSON.stringify(sel.name)}) was terminated twice; ` +
        "each selector records exactly one op",
      { selector: sel.selector, name: sel.name },
    );
  }
  sel.terminated = true;
}

function compact<T extends Record<string, unknown>>(obj: T): T {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function requireString(v: unknown, what: string): asserts v is string {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

function requireStrictness(v: unknown, what: string): TableStrictness | undefined {
  if (v === undefined) return undefined;
  if (v !== "strict" && v !== "lenient" && v !== "off") {
    throw structuredError("OP_INVALID", `${what} must be \"strict\", \"lenient\", or \"off\"`);
  }
  return v;
}

function requireOptionalBoolean(v: unknown, what: string): boolean | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "boolean") {
    throw structuredError("OP_INVALID", `${what} must be a boolean`);
  }
  return v;
}

function runtimeOptionsFromCreateArgs(args: CreateTableArgs): Node | undefined {
  const softDelete = requireOptionalBoolean(args.softDelete, "create({ softDelete })");
  const versioning = requireOptionalBoolean(args.versioning, "create({ versioning })");
  const strictness = requireStrictness(args.strictness, "create({ strictness })");
  const hasOptions =
    softDelete !== undefined ||
    versioning !== undefined ||
    strictness !== undefined;
  if (!hasOptions) return undefined;
  return compact({
    softDelete: softDelete ?? false,
    versioning: versioning ?? false,
    strictness: strictness ?? "strict",
  });
}

function runtimeOptionsPatchFromArgs(args: {
  softDelete?: boolean;
  versioning?: boolean;
  strictness?: TableStrictness;
}): Node {
  const softDelete = requireOptionalBoolean(args.softDelete, "setOptions({ softDelete })");
  const versioning = requireOptionalBoolean(args.versioning, "setOptions({ versioning })");
  const strictness = requireStrictness(args.strictness, "setOptions({ strictness })");
  const patch = compact({
    softDelete,
    versioning,
    strictness,
  });
  if (Object.keys(patch).length === 0) {
    throw structuredError(
      "OP_INVALID",
      "setOptions(...) must set at least one of softDelete, versioning, or strictness",
    );
  }
  return patch;
}

function requireSafeI64(v: unknown, what: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isSafeInteger(v)) {
    throw structuredError("OP_INVALID", `${what} must be a JS safe integer; got ${v}`);
  }
  return v;
}

function requireNullableSafeI64(v: unknown, what: string): number | null | undefined {
  if (v === null) return null;
  return requireSafeI64(v, what);
}

function requireSequenceIncrement(v: unknown, what: string): number | undefined {
  const n = requireSafeI64(v, what);
  if (n === 0) {
    throw structuredError("OP_INVALID", `${what} must be non-zero`);
  }
  return n;
}

function requireSequenceCache(v: unknown, what: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 1) {
    throw structuredError("OP_INVALID", `${what} must be a positive JS safe integer; got ${v}`);
  }
  return v;
}

function requireSequenceBounds(min: number | null | undefined, max: number | null | undefined, what: string): void {
  if (typeof min === "number" && typeof max === "number" && min > max) {
    throw structuredError("OP_INVALID", `${what}: minValue must be <= maxValue`);
  }
}

// ── (B) The IMMUTABLE chainable `t.*` lexicon (§4) ──

/** The CLOSED pgvector distance-metric token set (P2a §4) — the camelCase wire
 *  spelling of the engine's `VectorMetric` enum. Mirrored here (lock-step with
 *  `migrate_ops.js`) so `t.vector(n, { metric })` rejects an out-of-set metric
 *  with a friendly client-side OP_INVALID; the engine's closed enum stays
 *  authoritative. */
export const VECTOR_METRICS: readonly VectorMetric[] = ["cosine", "l2", "innerProduct"];
// The closed set of integer types a sequence may be declared `AS` (PG renders
// only `integer` / `bigint`; everything else fails late at lower as UnsupportedOp).
export const SEQUENCE_AS_TYPES: readonly ColType[] = ["int", "bigInt"];

/** The CLOSED column-mask token sets (#174) — the SDK/IR WIRE spelling of the
 *  engine's `IrMaskKind` / `IrClassification` enums. The two date kinds are KEBAB
 *  (`date-year`/`date-decade`); the rest are single camelCase words. Mirrored here
 *  (lock-step with `migrate_ops.js`) so `.mask({ kind, classification })` rejects an
 *  out-of-set token with a friendly client-side OP_INVALID; the engine's closed
 *  enums stay authoritative. */
export const MASK_KINDS: readonly MaskKind[] = [
  "full",
  "last4",
  "first4",
  "email",
  "name",
  "date-year",
  "date-decade",
  "none",
];
export const MASK_CLASSIFICATIONS: readonly Classification[] = [
  "public",
  "pii",
  "spi",
  "phi",
  "pci",
  "internal",
];

class ColumnDefImpl implements ColumnDefType {
  readonly _type: ColType;
  readonly _nullable: boolean;
  readonly _default: unknown;
  readonly _primaryKey: boolean;
  readonly _unique: boolean;
  // Migration-first P2a (§2b) declared-only facets carried on the IrColumn:
  // the typed-id prefix (`t.id({prefix})`) and the pgvector distance metric
  // (`t.vector(n, {metric})`). #174: a standalone column mask (`.mask({…})`).
  // Absent ⇒ omitted on the wire.
  readonly _idPrefix: string | undefined;
  readonly _vectorMetric: string | undefined;
  readonly _mask: { kind: string; classification: string } | undefined;
  readonly _generated: { expr: Node; stored: boolean } | undefined;
  readonly _identity: { always: boolean } | undefined;

  constructor(
    colType: ColType,
    fields?: {
      nullable?: boolean;
      default?: unknown;
      primaryKey?: boolean;
      unique?: boolean;
      idPrefix?: string;
      vectorMetric?: string;
      mask?: { kind: string; classification: string };
      generated?: { expr: Node; stored: boolean };
      identity?: { always: boolean };
    },
  ) {
    this._type = colType;
    this._nullable = fields?.nullable ?? true;
    this._default = fields?.default;
    this._primaryKey = fields?.primaryKey ?? false;
    this._unique = fields?.unique ?? false;
    this._idPrefix = fields?.idPrefix;
    this._vectorMetric = fields?.vectorMetric;
    this._mask = fields?.mask;
    this._generated = fields?.generated;
    this._identity = fields?.identity;
  }

  /** Clone with the named fields overridden — the basis of immutability (§4). */
  private with(over: {
    type?: ColType;
    nullable?: boolean;
    default?: unknown;
    primaryKey?: boolean;
    unique?: boolean;
    idPrefix?: string;
    vectorMetric?: string;
    mask?: { kind: string; classification: string };
    generated?: { expr: Node; stored: boolean };
    identity?: { always: boolean };
  }): ColumnDefImpl {
    return new ColumnDefImpl(over.type ?? this._type, {
      nullable: over.nullable ?? this._nullable,
      default: "default" in over ? over.default : this._default,
      primaryKey: over.primaryKey ?? this._primaryKey,
      unique: over.unique ?? this._unique,
      idPrefix: "idPrefix" in over ? over.idPrefix : this._idPrefix,
      vectorMetric: "vectorMetric" in over ? over.vectorMetric : this._vectorMetric,
      mask: "mask" in over ? over.mask : this._mask,
      generated: "generated" in over ? over.generated : this._generated,
      identity: "identity" in over ? over.identity : this._identity,
    });
  }

  /** Internal: carry the typed-id prefix (`t.id({prefix})`). */
  __withIdPrefix(prefix: string): ColumnDefImpl {
    return this.with({ idPrefix: prefix });
  }
  /** Internal: carry the pgvector distance metric (`t.vector(n, {metric})`). */
  __withVectorMetric(metric: string): ColumnDefImpl {
    return this.with({ vectorMetric: metric });
  }

  notNull(): ColumnDefImpl {
    return this.with({ nullable: false });
  }
  default(value: DefaultValue): ColumnDefImpl {
    return this.with({ default: toIrDefault(value) });
  }
  ref(targetTable: string): ColumnDefImpl {
    requireString(targetTable, "t.*.ref(target)");
    return this.with({ type: { ref: { references: targetTable } } as ColType });
  }
  primaryKey(): ColumnDefImpl {
    return this.with({ primaryKey: true, nullable: false });
  }
  unique(): ColumnDefImpl {
    return this.with({ unique: true });
  }

  /** `.mask({ kind, classification? })` (#174) — declare a STANDALONE column mask so
   *  the field reads back as `MaskedValue<T>` and the op lower emits the `__zsmask`
   *  sentinel + `_masked` sibling. `kind` is REQUIRED (closed `MASK_KINDS`);
   *  `classification` is optional and DEFAULTS to `"pii"` (closed
   *  `MASK_CLASSIFICATIONS`). The closed-set checks mirror `t.vector(n, { metric })`:
   *  a friendly client-side OP_INVALID over the SAME closed set the engine's enums
   *  enforce authoritatively. */
  mask(opts: MaskOptions): ColumnDefImpl {
    if (opts === null || typeof opts !== "object") {
      throw structuredError("OP_INVALID", "t.*.mask(opts): opts must be { kind, classification? }");
    }
    requireString(opts.kind, "t.*.mask({ kind })");
    if (!MASK_KINDS.includes(opts.kind)) {
      throw structuredError(
        "OP_INVALID",
        `t.*.mask({ kind }): kind must be one of ${MASK_KINDS.join(" | ")}, ` +
          `got ${JSON.stringify(opts.kind)}`,
        { kind: opts.kind },
      );
    }
    const classification = opts.classification === undefined ? "pii" : opts.classification;
    if (!MASK_CLASSIFICATIONS.includes(classification)) {
      throw structuredError(
        "OP_INVALID",
        `t.*.mask({ classification }): classification must be one of ` +
          `${MASK_CLASSIFICATIONS.join(" | ")}, got ${JSON.stringify(classification)}`,
        { classification },
      );
    }
    return this.with({ mask: { kind: opts.kind, classification } });
  }

  generated(expr: ExprFn | ExprChainType | Expr, opts?: GeneratedOptions): ColumnDefImpl {
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.generated(expr, opts): opts must be { virtual?: boolean }");
    }
    if (opts?.virtual !== undefined && typeof opts.virtual !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.generated(expr, { virtual }): virtual must be a boolean");
    }
    return this.with({
      generated: { expr: resolveExpr(expr as ExprFn | ExprChainType | Node)!, stored: opts?.virtual === true ? false : true },
    });
  }

  identity(opts?: IdentityOptions): ColumnDefImpl {
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.identity(opts): opts must be { always?: boolean }");
    }
    if (opts?.always !== undefined && typeof opts.always !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.identity({ always }): always must be a boolean");
    }
    return this.with({ identity: { always: opts?.always === true } });
  }

  __toIrColumn(name: string): Node {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      // C2 — a PRIMARY KEY already IMPLIES uniqueness, so a column that is BOTH
      // `.unique()` and `.primaryKey()` would otherwise carry a redundant
      // column-level UNIQUE (an extra index/constraint) on top of the table's pk
      // constraint. Suppress it (lock-step with the addColumn path + the differ,
      // which never emits a separate UNIQUE for the PK column).
      unique: this._unique && !this._primaryKey ? true : undefined,
      // P2a/#174 — carry the declared-only facets onto the wire IrColumn (camelCase
      // keys `idPrefix`/`vectorMetric`/`mask`, lock-step with `migrate_ops.js`).
      // Absent ⇒ omitted (compact), so a plain column is byte-identical to the
      // pre-facet image (checksum-neutral).
      idPrefix: this._idPrefix,
      vectorMetric: this._vectorMetric,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity,
    });
  }
  __toAddColumnTail(): Node {
    // #173: a typed-id prefix on an ADDED column is meaningless (an added column is
    // never the system PK) — `Op::AddColumn` has no `idPrefix` slot. Carrying it
    // would SILENTLY drop the prefix on the wire (the one outcome the closed-contract
    // discipline forbids); REFUSE it with a structured OP_INVALID, lock-step with
    // `migrate_ops.js`.
    if (this._idPrefix !== undefined) {
      throw structuredError(
        "OP_INVALID",
        "a t.id({ prefix }) typed-id prefix can only be declared in create(); an added " +
          "column is never the system primary key, so an addColumn carries no prefix slot",
        { facet: "idPrefix" },
      );
    }
    return compact({
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      // #173: carry the vector metric + standalone mask onto the addColumn op tail
      // (camelCase keys, lock-step with `Op::AddColumn`). Absent ⇒ omitted (compact).
      vectorMetric: this._vectorMetric,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity,
    });
  }
}

function isColumnDef(x: unknown): x is ColumnDefImpl {
  return x instanceof ColumnDefImpl;
}

/** Base64-encode raw bytes (the `IrScalar::Bytes` wire carrier) without a Node
 *  `Buffer` dependency — runs identically in the V8 record host and Node. */
function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]!);
  // `btoa` is a WHATWG global present in V8 + Node ≥16.
  return btoa(bin);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== "object") return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

function isExplicitScalarCarrier(value: Record<string, unknown>): boolean {
  const keys = Object.keys(value);
  return (
    keys.length === 1 &&
    ((keys[0] === "decimal" && typeof value.decimal === "string") ||
      (keys[0] === "bytes" && typeof value.bytes === "string"))
  );
}

function rejectNestedFunctionValues(value: unknown): void {
  rejectFunctionValue(value);
  if (Array.isArray(value)) {
    for (const item of value) rejectNestedFunctionValues(item);
  } else if (isPlainObject(value)) {
    for (const item of Object.values(value)) rejectNestedFunctionValues(item);
  }
}

/**
 * Normalize a JS scalar into the closed `IrScalar` WIRE carrier so the recorded
 * shape is exactly what Rust's `IrScalar` deserializer accepts (§3.5):
 *  - a JS `bigint` → `{ decimal: "<v>" }` (a bare bigint THROWS at JSON.stringify);
 *  - a `Uint8Array` → `{ bytes: "<base64>" }` (the raw-bytes carrier);
 *  - JSON containers are scanned so function values fail closed at any depth;
 *  - everything else passes through verbatim.
 */
function toIrScalar(value: unknown): unknown {
  rejectNestedFunctionValues(value);
  if (typeof value === "bigint") return { decimal: value.toString() };
  if (typeof value === "number" && Number.isFinite(value) && !Number.isInteger(value)) {
    return { decimal: String(value) };
  }
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  return value;
}

function toIrValue(value: unknown): unknown {
  const synth = nativeFnSynthNode(value);
  if (synth !== undefined) return synth;
  rejectFunctionValue(value);
  if (value instanceof ExprChainImpl) return value.__node;
  if (value && typeof value === "object" && typeof (value as Node).node === "string") return value as Node;
  return toIrScalar(value);
}

const NON_EMPTY_CONTAINER_DEFAULT_ERROR =
  "non-empty container defaults are not supported yet; only {} and [] are";

function toIrDefault(value: DefaultValue): Node {
  const fn = nativeFnSynthName(value);
  if (fn !== undefined) return { fn: { fn } };
  rejectFunctionValue(value);
  if (value && typeof value === "object" && "fn" in value && typeof value.fn === "string") {
    return { fn: { fn: value.fn } };
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return { container: "array" };
    rejectNestedFunctionValues(value);
    throw structuredError("OP_INVALID", NON_EMPTY_CONTAINER_DEFAULT_ERROR);
  }
  if (isPlainObject(value)) {
    if (Object.keys(value).length === 0) return { container: "object" };
    if (isExplicitScalarCarrier(value)) return { literal: { value: toIrScalar(value) } };
    rejectNestedFunctionValues(value);
    throw structuredError("OP_INVALID", NON_EMPTY_CONTAINER_DEFAULT_ERROR);
  }
  return { literal: { value: toIrScalar(value) } };
}

export const t: TypeLexicon = {
  id: (opts?: IdOptions) => {
    let col = new ColumnDefImpl("uuid").primaryKey().default({ fn: "genRandomUuid" });
    if (opts && opts.prefix !== undefined) {
      requireString(opts.prefix, "t.id({ prefix })");
      col = col.__withIdPrefix(opts.prefix);
    }
    return col;
  },
  text: () => new ColumnDefImpl("text"),
  textArray: () => new ColumnDefImpl("textArray"),
  numeric: (precision = 38, scale = 9) =>
    new ColumnDefImpl({ decimal: { precision, scale } } as ColType),
  char: (n) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.char(n): n must be a positive integer, got ${n}`);
    }
    return new ColumnDefImpl({ char: { len: n } } as ColType);
  },
  timestamp: () => new ColumnDefImpl("timestamp"),
  date: () => new ColumnDefImpl("date" as ColType),
  uuid: () => new ColumnDefImpl("uuid"),
  bytes: () => new ColumnDefImpl("bytea"),
  boolean: () => new ColumnDefImpl("bool"),
  json: () => new ColumnDefImpl("json"),
  ref: (targetTable) => {
    requireString(targetTable, "t.ref(target)");
    return new ColumnDefImpl({ ref: { references: targetTable } } as ColType);
  },
  vector: (n, opts?: VectorOptions) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    let col = new ColumnDefImpl({ vector: { vector: n } } as ColType);
    if (opts && opts.metric !== undefined) {
      requireString(opts.metric, "t.vector(n, { metric })");
      // LOW-1: a closed-set check on the metric token gives a friendly OP_INVALID at
      // authoring time instead of a cryptic serde "unknown variant" at the Rust
      // deserialize seam (the engine's closed `VectorMetric` enum stays authoritative).
      if (!VECTOR_METRICS.includes(opts.metric)) {
        throw structuredError(
          "OP_INVALID",
          `t.vector(n, { metric }): metric must be one of ${VECTOR_METRICS.join(" | ")}, ` +
            `got ${JSON.stringify(opts.metric)}`,
          { metric: opts.metric },
        );
      }
      col = col.__withVectorMetric(opts.metric);
    }
    return col;
  },
  geoPoint: () => new ColumnDefImpl("geoPoint"),
  smallInt: () => new ColumnDefImpl("smallInt"),
  integer: () => new ColumnDefImpl("int"),
  int: () => new ColumnDefImpl("int"),
  bigInt: () => new ColumnDefImpl("bigInt"),
  real: () => new ColumnDefImpl("real"),
  float: () => new ColumnDefImpl("float"),
  inet: () => new ColumnDefImpl("inet"),
  enum: (name) => {
    const n = typeof name === "string" ? name : name.name;
    requireString(n, "t.enum(name)");
    return new ColumnDefImpl({ enum: { name: n } } as ColType);
  },
  domain: (name) => {
    const n = typeof name === "string" ? name : name.name;
    requireString(n, "t.domain(name)");
    return new ColumnDefImpl({ domain: { name: n } } as ColType);
  },
  encrypted: (arg) => {
    const inner = arg && typeof arg === "object" && "of" in arg ? (arg as { of: unknown }).of : arg;
    const innerType = isColumnDef(inner) ? inner._type : (inner as ColType);
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return new ColumnDefImpl({ encrypted: { of: innerType } } as ColType);
  },
};

function colTypeOf(typeArg: ColumnDefType | ColType): ColType {
  if (isColumnDef(typeArg)) return typeArg._type;
  return typeArg as ColType;
}

export function enumType(name: string): EnumHandle {
  requireString(name, "enumType(name)");
  const handle: EnumHandle = {
    name,
    create(createArgs: CreateEnumArgs) {
      recordCreateEnum(name, createArgs);
      return handle;
    },
    drop(dropArgs: DropEnumArgs = {}) {
      recordDropEnum(name, dropArgs);
      return handle;
    },
    comment(text: string | null, commentArgs: { schema?: string } = {}) {
      recordComment({ kind: "type", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

export function __pgDomain(name: string): DomainHandle {
  requireString(name, "domain(name)");
  const handle: DomainHandle = {
    name,
    create(args: CreateDomainArgs) {
      recordCreateDomain(name, args);
      return handle;
    },
    drop(args: DropDomainArgs = {}) {
      recordDropDomain(name, args);
      return handle;
    },
    comment(text: string | null, commentArgs: { schema?: string } = {}) {
      recordComment({ kind: "type", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

export function __pgSequence(name: string): SequenceHandle {
  requireString(name, "sequence(name)");
  const handle: SequenceHandle = {
    name,
    create(args: CreateSequenceArgs = {}) {
      recordCreateSequence(name, args);
      return handle;
    },
    alter(args: AlterSequenceArgs) {
      recordAlterSequence(name, args);
      return handle;
    },
    drop(args: DropSequenceArgs = {}) {
      recordDropSequence(name, args);
      return handle;
    },
    comment(text: string | null, commentArgs: { schema?: string } = {}) {
      recordComment({ kind: "sequence", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

// ── (A) The shared `@zeroship/db` lexicon bridge (PR5) ──

/**
 * Lift a `@zeroship/db` schema field (a `t.*` `TypeBuilder` or its `FieldDef`)
 * into a migration `ColumnDef`, so a column declared in the live `@zeroship/db`
 * schema lowers through the IDENTICAL `ColType` path a hand-written migration
 * column does (PR5 goal A — one shared lexicon). The TYPE is bridged via the
 * single-source {@link colTypeFromDbField} reduction; the column's NULLABILITY is
 * carried over (`@zeroship/db` `.required()` → migration `.notNull()`). Table/
 * column NAMES are NEVER bound to the live schema. Returns a chainable
 * (immutable) `ColumnDef`, so a caller can still layer migration modifiers on top.
 */
export function fromDb(field: DbSchemaField): ColumnDefType {
  let def: ColumnDefImpl = new ColumnDefImpl(colTypeFromDbField(field));
  const fd = field instanceof DbTypeBuilder ? field.toFieldDef() : field;
  if (fd && typeof fd === "object" && (fd as { required?: boolean }).required === true) {
    def = def.notNull();
  }
  if (fd && typeof fd === "object" && (fd as { unique?: boolean }).unique === true) {
    def = def.unique();
  }
  return def;
}

// ── (B) The fluent `(c) => Expr` builder (§3.6) ──

function chain(node: Node): ExprChainImpl {
  return new ExprChainImpl(node);
}

function exprArg(x: unknown): Node {
  const synth = nativeFnSynthNode(x);
  if (synth !== undefined) return synth;
  rejectFunctionValue(x);
  if (x instanceof ExprChainImpl) return x.__node;
  if (x && typeof x === "object" && typeof (x as Node).node === "string") return x as Node;
  return { node: "literal", value: toIrScalar(x) };
}

function textLiteralArray(values: unknown, what: string): string[] {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a string[]`);
  }
  if (values.length === 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-empty string[]`);
  }
  return values.map((v, i) => {
    if (typeof v !== "string") {
      throw structuredError("OP_INVALID", `${what}[${i}] must be a string; got ${typeof v}`);
    }
    if (v.length === 0) {
      throw structuredError("OP_INVALID", `${what}[${i}] must be non-empty`);
    }
    if (v.includes("\0")) {
      throw structuredError("OP_INVALID", `${what}[${i}] must not contain a NUL byte`);
    }
    return v;
  });
}

function pgRegexPattern(pattern: unknown): string {
  if (typeof pattern !== "string") {
    throw structuredError("OP_INVALID", `c.pg.regex(pattern): pattern must be a string; got ${typeof pattern}`);
  }
  if (pattern.length === 0) {
    throw structuredError("OP_INVALID", "c.pg.regex(pattern): pattern must be non-empty");
  }
  if (pattern.includes("\0")) {
    throw structuredError("OP_INVALID", "c.pg.regex(pattern): pattern must not contain a NUL byte");
  }
  return pattern;
}

function pgExtractField(field: unknown): "day" {
  if (field !== "day") {
    throw structuredError("OP_INVALID", `c.pg.extract(field, expr): field must be "day"; got ${JSON.stringify(field)}`);
  }
  return field;
}

function pgIntervalLiteral(value: unknown): string {
  if (typeof value !== "string") {
    throw structuredError("OP_INVALID", `c.pg.interval(value): value must be a string; got ${typeof value}`);
  }
  if (!isSafePgIntervalLiteral(value)) {
    throw structuredError(
      "OP_INVALID",
      `c.pg.interval(value): value must match HH:MM:SS or HH:MM:SS.ffffff; got ${JSON.stringify(value)}`,
    );
  }
  return value;
}

function isSafePgIntervalLiteral(value: string): boolean {
  const m = /^([0-9]{1,6}):([0-9]{2}):([0-9]{2})(?:\.([0-9]{1,6}))?$/.exec(value);
  if (!m) return false;
  return Number(m[2]) <= 59 && Number(m[3]) <= 59;
}

class ExprChainImpl implements ExprChainType {
  __node: Node;
  constructor(node: Node) {
    this.__node = node;
  }
  private bin(op: string, x: unknown): ExprChainImpl {
    return chain({ node: "binOp", op, lhs: this.__node, rhs: exprArg(x) });
  }
  eq(x: unknown) { return this.bin("eq", x); }
  ne(x: unknown) { return this.bin("ne", x); }
  lt(x: unknown) { return this.bin("lt", x); }
  le(x: unknown) { return this.bin("le", x); }
  gt(x: unknown) { return this.bin("gt", x); }
  ge(x: unknown) { return this.bin("ge", x); }
  and(e: unknown) { return this.bin("and", e); }
  or(e: unknown) { return this.bin("or", e); }
  not() { return chain({ node: "unaryOp", op: "not", operand: this.__node }); }
  add(x: unknown) { return this.bin("add", x); }
  sub(x: unknown) { return this.bin("sub", x); }
  mul(x: unknown) { return this.bin("mul", x); }
  div(x: unknown) { return this.bin("div", x); }
  concat(...parts: unknown[]) {
    let acc = this.__node;
    for (const p of parts) acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    return chain(acc);
  }
  isNull() { return chain({ node: "unaryOp", op: "isNull", operand: this.__node }); }
  isNotNull() { return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node }); }
  isTrue() { return chain({ node: "unaryOp", op: "isTrue", operand: this.__node }); }
  isFalse() { return chain({ node: "unaryOp", op: "isFalse", operand: this.__node }); }
  matches(pattern: string) {
    return chain({ node: "pgRegexMatch", expr: this.__node, pattern: pgRegexPattern(pattern) });
  }
  columnSize() {
    return chain({ node: "pgColumnSize", expr: this.__node });
  }
  cast(target: "text" | "integer" | "real" | "boolean" | "blob" | "uuid") {
    return chain({ node: "cast", operand: this.__node, target });
  }
}

function foldExprs(op: "and" | "or", exprs: readonly unknown[], what: string): ExprChainImpl {
  if (!Array.isArray(exprs) || exprs.length === 0) {
    throw structuredError("OP_INVALID", `${what} requires at least one expression`);
  }
  let acc = exprArg(exprs[0]);
  for (const expr of exprs.slice(1)) {
    acc = { node: "binOp", op, lhs: acc, rhs: exprArg(expr) };
  }
  return chain(acc);
}

export function check(name: string, expr: ExprFn): CheckDef {
  requireString(name, "check(name, expr)");
  if (typeof expr !== "function") {
    throw structuredError("OP_INVALID", "check(name, expr): expr must be a (c) => Expr callback");
  }
  return { name, expr };
}

export function and(...exprs: unknown[]): ExprChainType {
  return foldExprs("and", exprs, "and(...)");
}

export function or(...exprs: unknown[]): ExprChainType {
  return foldExprs("or", exprs, "or(...)");
}

export function not(expr: unknown): ExprChainType {
  return chain({ node: "unaryOp", op: "not", operand: exprArg(expr) });
}

export function membership(expr: unknown, values: readonly string[]): ExprChainType {
  return chain({
    node: "pgArrayMembership",
    expr: exprArg(expr),
    op: "eq",
    elems: textLiteralArray(values, "membership(values)"),
  });
}

export function notMembership(expr: unknown, values: readonly string[]): ExprChainType {
  return chain({
    node: "pgArrayMembership",
    expr: exprArg(expr),
    op: "ne",
    elems: textLiteralArray(values, "notMembership(values)"),
  });
}

export function lit(value: ScalarValue): ExprChainType {
  return chain({ node: "literal", value: toIrScalar(value) });
}

export function interval(value: string): ExprChainType {
  return chain({ node: "pgIntervalLiteral", value: pgIntervalLiteral(value) });
}

const fn: FnNamespace = {
  lower: (e) => chain({ node: "fnCall", fn: "lower", args: [exprArg(e)] }),
  upper: (e) => chain({ node: "fnCall", fn: "upper", args: [exprArg(e)] }),
  trim: (e) => chain({ node: "fnCall", fn: "trim", args: [exprArg(e)] }),
  length: (e) => chain({ node: "fnCall", fn: "length", args: [exprArg(e)] }),
  abs: (e) => chain({ node: "fnCall", fn: "abs", args: [exprArg(e)] }),
  coalesce: (...args) => chain({ node: "fnCall", fn: "coalesce", args: args.map(exprArg) }),
  nullif: (a, b) => chain({ node: "fnCall", fn: "nullif", args: [exprArg(a), exprArg(b)] }),
  currentSetting: (name, missingOk) =>
    chain({
      node: "fnCall",
      fn: "currentSetting",
      args: missingOk === undefined
        ? [{ node: "literal", value: name }]
        : [{ node: "literal", value: name }, { node: "literal", value: missingOk }],
    }),
  currentUser: () => chain({ node: "fnCall", fn: "currentUser", args: [] }),
  concatWs: (sep, ...parts) => chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),
  case: (branches, elseVal) => {
    if (!Array.isArray(branches)) {
      throw structuredError("OP_INVALID", "c.fn.case(branches, else?): branches must be an array of [cond, result]");
    }
    const node: Node = {
      node: "case",
      branches: branches.map((b) => {
        if (!Array.isArray(b) || b.length !== 2) {
          throw structuredError("OP_INVALID", "c.fn.case branch must be a [condition, result] pair");
        }
        return { condition: exprArg(b[0]), result: exprArg(b[1]) };
      }),
    };
    if (elseVal !== undefined) node.else = exprArg(elseVal);
    return chain(node);
  },
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },
  now: () => chain({ node: "fnSynth", fn: "now", args: [] }),
  genRandomUuid: () => chain({ node: "fnSynth", fn: "genRandomUuid", args: [] }),
};

const pgExpr: PgExprNamespace = {
  eqAnyArray: (expr, elems) => chain({
    node: "pgArrayMembership",
    expr: exprArg(expr),
    op: "eq",
    elems: textLiteralArray(elems, "c.pg.eqAnyArray(elems)"),
  }),
  neAllArray: (expr, elems) => chain({
    node: "pgArrayMembership",
    expr: exprArg(expr),
    op: "ne",
    elems: textLiteralArray(elems, "c.pg.neAllArray(elems)"),
  }),
  regex: (expr, pattern) => chain({
    node: "pgRegexMatch",
    expr: exprArg(expr),
    pattern: pgRegexPattern(pattern),
  }),
  columnSize: (expr) => chain({ node: "pgColumnSize", expr: exprArg(expr) }),
  extract: (field, expr) => chain({
    node: "extract",
    field: pgExtractField(field),
    expr: exprArg(expr),
  }),
  interval: (value) => chain({
    node: "pgIntervalLiteral",
    value: pgIntervalLiteral(value),
  }),
};

function makeBuilder(): ExprBuilder {
  const c = ((name: string) => {
    requireString(name, 'c("name")');
    return chain({ node: "colRef", name });
  }) as unknown as ExprBuilder;
  c.col = c;
  c.fn = fn;
  c.pg = pgExpr;
  return c;
}

function resolveExpr(slot: ExprFn | ExprChainType | Node | undefined): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  if (typeof slot === "function") return exprArg(slot(makeBuilder()));
  if (slot instanceof ExprChainImpl) return slot.__node;
  if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") return slot as Node;
  throw structuredError("OP_INVALID", "expression slot must be a (c) => Expr callback or a built expression");
}

/** Internal hook used only by the `@zeroship/migrate/pg` subpath. */
export function __pgResolveExpr(slot: ExprFn | ExprChainType | Expr | undefined): Node | undefined {
  return resolveExpr(slot as ExprFn | ExprChainType | Node | undefined);
}

function resolveSet(set: Record<string, ExprFn>): Record<string, Node> {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column → expression");
  }
  const out: Record<string, Node> = {};
  for (const col of Object.keys(set)) out[col] = resolveExpr(set[col])!;
  return out;
}

// ── (C) Existence-guard token mappers ──

function ifNotExistsGuard(v: boolean | undefined): "ifNotExists" | undefined {
  return v ? "ifNotExists" : undefined;
}
function ifExistsGuard(v: boolean | undefined): "ifExists" | undefined {
  return v ? "ifExists" : undefined;
}

function stringArray(values: unknown, what: string): string[] {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a string[]`);
  }
  for (const v of values) requireString(v, what);
  return [...values];
}

const PARTITION_BOUND_SENTINEL = "__zeroshipPartitionBound";

export const minValue = Object.freeze({ [PARTITION_BOUND_SENTINEL]: "minValue" }) as PartitionBoundSentinel;
export const maxValue = Object.freeze({ [PARTITION_BOUND_SENTINEL]: "maxValue" }) as PartitionBoundSentinel;

function partitionSpec(kind: PartitionSpec["kind"], columns: readonly string[], what: string): PartitionSpec {
  return { kind, columns: stringArray(columns, what) } as PartitionSpec;
}

export const p: PartitionBuilder = Object.freeze({
  range(columns: readonly string[]) {
    return partitionSpec("range", columns, "p.range(columns)");
  },
  list(columns: readonly string[]) {
    return partitionSpec("list", columns, "p.list(columns)");
  },
  hash(columns: readonly string[]) {
    return partitionSpec("hash", columns, "p.hash(columns)");
  },
});

function partitionSpecToIr(spec: PartitionSpec | undefined, what: string): Node | undefined {
  if (spec === undefined) return undefined;
  if (!spec || typeof spec !== "object") {
    throw structuredError("OP_INVALID", `${what} must be built with p.range/list/hash`);
  }
  if (spec.kind !== "range" && spec.kind !== "list" && spec.kind !== "hash") {
    throw structuredError("OP_INVALID", `${what}.kind must be "range", "list", or "hash"`);
  }
  return partitionSpec(spec.kind, spec.columns, `${what}.columns`);
}

function requireU32(v: unknown, what: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 0 || v > 0xffffffff) {
    throw structuredError("OP_INVALID", `${what} must be a u32 integer; got ${v}`);
  }
  return v;
}

function partitionBoundValueToIr(value: PartitionBoundInput, what: string): Node {
  if (value === minValue) return { kind: "minValue" };
  if (value === maxValue) return { kind: "maxValue" };
  if (typeof value === "string") return { kind: "string", value };
  if (typeof value === "number" && Number.isSafeInteger(value)) {
    return { kind: "int", value };
  }
  throw structuredError(
    "OP_INVALID",
    `${what} must be a string, JS safe integer, minValue, or maxValue`,
  );
}

function partitionBoundListToIr(values: unknown, what: string): Node[] {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be an array`);
  }
  return values.map((value, i) => partitionBoundValueToIr(value as PartitionBoundInput, `${what}[${i}]`));
}

function partitionBoundsFromForValues(args: unknown): Node {
  if (!args || typeof args !== "object") {
    throw structuredError(
      "OP_INVALID",
      "partition(name).of(parent).forValues(args) needs a bounds object",
    );
  }
  const bounds = args as { from?: unknown; to?: unknown; in?: unknown; modulus?: unknown; remainder?: unknown };
  const hasRange = bounds.from !== undefined || bounds.to !== undefined;
  const hasList = bounds.in !== undefined;
  const hasHash = bounds.modulus !== undefined || bounds.remainder !== undefined;
  const variantCount = (hasRange ? 1 : 0) + (hasList ? 1 : 0) + (hasHash ? 1 : 0);
  if (variantCount !== 1) {
    throw structuredError(
      "OP_INVALID",
      "partition bounds must be exactly one of { from, to }, { in }, or { modulus, remainder }",
    );
  }
  if (hasRange) {
    return {
      kind: "range",
      from: partitionBoundListToIr(bounds.from, "partition bounds.from"),
      to: partitionBoundListToIr(bounds.to, "partition bounds.to"),
    };
  }
  if (hasList) {
    return {
      kind: "list",
      values: partitionBoundListToIr(bounds.in, "partition bounds.in"),
    };
  }
  return {
    kind: "hash",
    modulus: requireU32(bounds.modulus, "partition bounds.modulus"),
    remainder: requireU32(bounds.remainder, "partition bounds.remainder"),
  };
}

function indexIncludeToIr(include: readonly string[] | undefined): string[] | undefined {
  if (include === undefined) return undefined;
  const cols = stringArray(include, "index include");
  return cols.length === 0 ? undefined : cols;
}

function indexWithToIr(params: IndexStorageParamsArg | undefined): Node | undefined {
  if (params === undefined) return undefined;
  if (!params || typeof params !== "object") {
    throw structuredError("OP_INVALID", "index with(...) must be an object");
  }
  const withParams = compact({
    pagesPerRange: requireU32(params.pagesPerRange, "index with.pagesPerRange"),
    fillfactor: requireU32(params.fillfactor, "index with.fillfactor"),
  });
  return Object.keys(withParams).length === 0 ? undefined : withParams;
}

function recordCreateEnum(name: string, args: CreateEnumArgs): void {
  requireString(name, "enumType(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "enumType(name).create({ values, ... }) needs an object");
  }
  const enumValues = stringArray(args.values, "enumType(name).create({ values })");
  if (enumValues.length === 0) {
    throw structuredError(
      "OP_INVALID",
      "enumType(name).create({ values }): values must be a non-empty string[] (an empty enum renders invalid SQL on MySQL/SQLite)",
    );
  }
  pushOrDeferUp(
    compact({
      op: "createEnum",
      name,
      schema: args.schema,
      values: enumValues,
    }),
  );
}

function recordDropEnum(name: string, args: DropEnumArgs = {}): void {
  requireString(name, "enumType(name).drop()");
  push(
    compact({
      op: "dropEnum",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateDomain(name: string, args: CreateDomainArgs): void {
  requireString(name, "domain(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "domain(name).create({ as, ... }) needs an object");
  }
  if (args.notNull !== undefined && typeof args.notNull !== "boolean") {
    throw structuredError("OP_INVALID", "domain(name).create({ notNull }): notNull must be a boolean");
  }
  pushOrDeferUp(
    compact({
      op: "createDomain",
      name,
      schema: args.schema,
      as: colTypeOf(args.as),
      check: resolveExpr(args.check as ExprFn | ExprChainType | Node | undefined),
      default: args.default === undefined ? undefined : toIrDefault(args.default),
      notNull: args.notNull,
    }),
  );
}

function recordDropDomain(name: string, args: DropDomainArgs = {}): void {
  requireString(name, "domain(name).drop()");
  push(
    compact({
      op: "dropDomain",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateSequence(name: string, args: CreateSequenceArgs = {}): void {
  requireString(name, "sequence(name)");
  if (args === null || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).create(args) needs an object");
  }
  const minValue = requireNullableSafeI64(args.minValue, "sequence.create({ minValue })");
  const maxValue = requireNullableSafeI64(args.maxValue, "sequence.create({ maxValue })");
  requireSequenceBounds(minValue, maxValue, "sequence.create(args)");
  const asType = args.as === undefined ? undefined : colTypeOf(args.as);
  if (asType !== undefined && !SEQUENCE_AS_TYPES.includes(asType)) {
    throw structuredError(
      "OP_INVALID",
      `sequence.create({ as }): as must be one of ${SEQUENCE_AS_TYPES.join(" | ")}; got ${JSON.stringify(asType)}`,
    );
  }
  pushOrDeferUp(
    compact({
      op: "createSequence",
      name,
      schema: args.schema,
      as: asType,
      increment: requireSequenceIncrement(args.increment, "sequence.create({ increment })"),
      start: requireSafeI64(args.start, "sequence.create({ start })"),
      minValue,
      maxValue,
      cache: requireSequenceCache(args.cache, "sequence.create({ cache })"),
      cycle: args.cycle,
      ownedBy: args.ownedBy,
    }),
  );
}

function recordAlterSequence(name: string, args: AlterSequenceArgs): void {
  requireString(name, "sequence(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).alter(args) needs an object");
  }
  const minValue = requireNullableSafeI64(args.minValue, "sequence.alter({ minValue })");
  const maxValue = requireNullableSafeI64(args.maxValue, "sequence.alter({ maxValue })");
  requireSequenceBounds(minValue, maxValue, "sequence.alter(args)");
  push(
    compact({
      op: "alterSequence",
      name,
      schema: args.schema,
      increment: requireSequenceIncrement(args.increment, "sequence.alter({ increment })"),
      restart: requireNullableSafeI64(args.restart, "sequence.alter({ restart })"),
      minValue,
      maxValue,
      cache: requireSequenceCache(args.cache, "sequence.alter({ cache })"),
      cycle: args.cycle,
      ownedBy: args.ownedBy,
    }),
  );
}

function recordDropSequence(name: string, args: DropSequenceArgs = {}): void {
  requireString(name, "sequence(name)");
  push(
    compact({
      op: "dropSequence",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordComment(target: CommentTargetArg, text: string | null): void {
  if (text !== null && typeof text !== "string") {
    throw structuredError("OP_INVALID", "comment text must be a string or null");
  }
  push(
    compact({
      op: "comment",
      target: commentTargetToIr(target),
      comment: text === null ? undefined : text,
    }),
  );
}

function commentTargetToIr(target: CommentTargetArg): Node {
  if (!target || typeof target !== "object") {
    throw structuredError("OP_INVALID", "comment target must be a closed target object");
  }
  switch (target.kind) {
    case "table":
    case "index":
    case "view":
    case "type":
    case "sequence":
    case "function":
      requireString(target.name, `comment target ${target.kind}.name`);
      return compact({ kind: target.kind, schema: target.schema, name: target.name });
    case "column":
    case "constraint":
      requireString(target.table, `comment target ${target.kind}.table`);
      requireString(target.name, `comment target ${target.kind}.name`);
      return compact({
        kind: target.kind,
        schema: target.schema,
        table: target.table,
        name: target.name,
      });
    default:
      throw structuredError("OP_INVALID", `unsupported comment target kind ${(target as { kind?: unknown }).kind}`);
  }
}

// ── (D) The internal op-construction helpers (the single source of truth) ──
//
// These build + push the EXACT canonical op object the Rust `Op` enum / the
// recorder twin emit (byte-identical IR except the C1 FK-actions delta). They are
// internal — only the fluent `table()` handle calls them.

function recordCreateTable(name: string, args: CreateTableArgs): void {
  const cols: Node[] = [];
  const constraints: Node[] = [];
  const indexes: Node[] = [];
  const pkCols: string[] = [];

  for (const colName of Object.keys(args.columns)) {
    const def = args.columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError("OP_INVALID", `create column "${colName}" must be a t.* ColumnDef`);
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  // An explicit `primaryKey` wins over per-column `.primaryKey()` collection:
  // undefined = unresolved policy default, null = explicit no-PK, string[] = author PK.
  const primaryKey = args.primaryKey !== undefined ? args.primaryKey : pkCols.length ? pkCols : undefined;

  for (const uq of args.uniques ?? []) {
    constraints.push(compact({ name: uq.name, kind: { kind: "unique", columns: uq.columns } }));
  }
  for (const ck of args.checks ?? []) {
    constraints.push(compact({ name: ck.name, kind: { kind: "check", expr: resolveExpr(ck.expr) } }));
  }
  for (const exclusion of args.exclusions ?? []) {
    constraints.push(exclusionConstraintFromSpec(exclusion));
  }
  for (const fkSpec of args.foreignKeys ?? []) {
    constraints.push(
      fkConstraintFromSpec({
        name: fkSpec.name,
        columns: fkSpec.columns,
        references: fkSpec.references,
        onDelete: fkSpec.onDelete,
        onUpdate: fkSpec.onUpdate,
        schema: args.schema,
      }),
    );
  }
  for (const idx of args.indexes ?? []) {
    indexes.push(
      compact({
        name: idx.name,
        columns: idx.columns.map(indexElementToIr),
        unique: idx.unique,
        using: idx.using,
        where: resolveExpr(idx.where),
        include: indexIncludeToIr(idx.include),
        with: indexWithToIr(idx.with),
        only: requireOptionalBoolean(idx.only, "index only"),
      }),
    );
  }

  push(
    compact({
      op: "createTable",
      name,
      columns: cols,
      primaryKey,
      constraints: constraints.length ? constraints : undefined,
      indexes: indexes.length ? indexes : undefined,
      partitionBy: partitionSpecToIr(args.partitionBy, "create({ partitionBy })"),
      runtimeOptions: runtimeOptionsFromCreateArgs(args),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordCreatePartition(
  name: string,
  parent: string,
  bounds: Node,
  args: { ifNotExists?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "createPartition",
      name,
      of: parent,
      bounds,
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDetachPartition(
  parent: string,
  name: string,
  args: { concurrently?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "detachPartition",
      parent,
      name,
      schema: args.schema,
      concurrently: args.concurrently,
    }),
  );
}

function recordDropPartition(
  name: string,
  args: { ifExists?: boolean; cascade?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "dropPartition",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
      cascade: args.cascade,
    }),
  );
}

function recordSetTableOptions(
  table: string,
  args: { softDelete?: boolean; versioning?: boolean; strictness?: TableStrictness; schema?: string },
): void {
  push(
    compact({
      op: "setTableOptions",
      table,
      options: runtimeOptionsPatchFromArgs(args),
      schema: args.schema,
    }),
  );
}

function recordDropTable(
  table: string,
  args: { ifExists?: boolean; cascade?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "dropTable",
      table,
      cascade: args.cascade,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordRenameTable(
  table: string,
  to: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "renameTable",
      table,
      to,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordAddColumn(
  table: string,
  column: string,
  type: ColumnDefImpl,
  args: { ifNotExists?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "addColumn",
      table,
      column,
      ...type.__toAddColumnTail(),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
  // C2 — `.column(x).add({ type: t.text().unique() })` honors `.unique()`: emit a
  // follow-on unique constraint (mirroring the createTable per-column `.unique()`
  // image, which rides the column's `unique:true` field — but an ADD COLUMN has no
  // inline UNIQUE, so it lowers to a separate ADD CONSTRAINT). Likewise a
  // `.primaryKey()` on an added column hoists a pk add.
  //
  // A PRIMARY KEY already IMPLIES uniqueness, so when BOTH are set the follow-on
  // UNIQUE is redundant DDL (an extra index/constraint) — suppress it when
  // `_primaryKey` is set, mirroring how the differ never emits a separate UNIQUE
  // for the PK column. Only the pk add is recorded.
  if (type._unique && !type._primaryKey) {
    push(
      compact({
        op: "addConstraint",
        table,
        constraint: { kind: { kind: "unique", columns: [column] } },
        schema: args.schema,
        existenceGuard: ifNotExistsGuard(args.ifNotExists),
      }),
    );
  }
  if (type._primaryKey) {
    push(
      compact({
        op: "addConstraint",
        table,
        constraint: { kind: { kind: "pk", columns: [column] } },
        schema: args.schema,
        existenceGuard: ifNotExistsGuard(args.ifNotExists),
      }),
    );
  }
}

function recordDropColumn(
  table: string,
  column: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "dropColumn",
      table,
      column,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordRenameColumn(
  table: string,
  from: string,
  to: string,
  type: ColumnDefType,
  args: { schema?: string },
): void {
  push(
    compact({
      op: "renameColumn",
      table,
      from,
      to,
      type: colTypeOf(type),
      schema: args.schema,
    }),
  );
}

function recordSetColumnType(
  table: string,
  name: string,
  change: { to: ColumnDefType; using?: ExprFn; schema?: string },
): void {
  requireColumnDef(change.to, ".column(name).setType({ to })");
  push(
    compact({
      op: "setColumnType",
      table,
      column: name,
      toType: colTypeOf(change.to),
      using: resolveExpr(change.using),
      schema: change.schema,
    }),
  );
}

function recordSetColumnNotNull(table: string, name: string, args: { schema?: string }): void {
  push(compact({ op: "setColumnNotNull", table, column: name, schema: args.schema }));
}

function recordDropColumnNotNull(table: string, name: string, args: { schema?: string }): void {
  push(compact({ op: "dropColumnNotNull", table, column: name, schema: args.schema }));
}

function recordSetColumnDefault(
  table: string,
  name: string,
  value: DefaultValue,
  args: { schema?: string },
): void {
  push(
    compact({
      op: "setColumnDefault",
      table,
      column: name,
      value: toIrDefault(value),
      schema: args.schema,
    }),
  );
}

function recordDropColumnDefault(table: string, name: string, args: { schema?: string }): void {
  push(compact({ op: "dropColumnDefault", table, column: name, schema: args.schema }));
}

/** Build an `IrConstraint` of kind `fk`. **C1**: `onDelete`/`onUpdate` ARE
 *  emitted (compacted — omitted when absent, so an action-free FK is byte-
 *  identical to the pre-C1 wire image). */
function fkConstraintFromSpec(spec: {
  name?: string;
  columns: string[];
  references: ForeignKeyReference;
  onDelete?: RefAction;
  onUpdate?: RefAction;
  schema?: string;
}): Node {
  if (!spec || typeof spec !== "object" || !spec.references) {
    throw structuredError("OP_INVALID", ".foreignKey(name).add needs { columns, references:{ table, columns } }");
  }
  if (spec.references.schema !== undefined) {
    requireString(spec.references.schema, "foreign key references.schema");
    if (spec.schema !== undefined && spec.references.schema !== spec.schema) {
      throw structuredError(
        "OP_INVALID",
        "foreign key references.schema must match the table schema; cross-schema FKs are not representable in the frozen IR",
      );
    }
  }
  return compact({
    name: spec.name,
    kind: compact({
      kind: "fk",
      columns: spec.columns,
      referencesTable: spec.references.table,
      referencesColumns: spec.references.columns,
      onDelete: spec.onDelete,
      onUpdate: spec.onUpdate,
    }),
  });
}

function recordAddForeignKey(
  table: string,
  name: string,
  args: {
    columns: string[];
    references: ForeignKeyReference;
    onDelete?: RefAction;
    onUpdate?: RefAction;
    ifNotExists?: boolean;
    schema?: string;
  },
): void {
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: fkConstraintFromSpec({
        name,
        columns: args.columns,
        references: args.references,
        onDelete: args.onDelete,
        onUpdate: args.onUpdate,
        schema: args.schema,
      }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordAddUnique(
  table: string,
  name: string,
  args: { columns: string[]; ifNotExists?: boolean; schema?: string },
): void {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".unique(name).add needs { columns: string[] }");
  }
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name, kind: { kind: "unique", columns: args.columns } }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordAddCheck(
  table: string,
  name: string,
  args: { expr: ExprFn; ifNotExists?: boolean; schema?: string },
): void {
  if (!args || args.expr === undefined) {
    throw structuredError("OP_INVALID", ".check(name).add needs { expr: (c) => Expr }");
  }
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: compact({ name, kind: { kind: "check", expr: resolveExpr(args.expr) } }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function exclusionConstraintFromSpec(
  spec: { name?: string } & ExclusionConstraintArgs,
): Node {
  if (!spec || typeof spec !== "object" || !Array.isArray(spec.elements)) {
    throw structuredError(
      "OP_INVALID",
      ".exclusion(name).add needs { elements: [{ target, operator }], ... }",
    );
  }
  return compact({
    name: spec.name,
    kind: compact({
      kind: "exclusion",
      usingMethod: spec.using,
      elements: spec.elements.map(exclusionElementToIr),
      wherePredicate: resolveExpr(spec.where as ExprFn | ExprChainType | Node | undefined),
      deferrable: spec.deferrable,
      initiallyDeferred: spec.initiallyDeferred,
    }),
  });
}

function exclusionElementToIr(element: ExclusionElementArg): Node {
  if (!element || typeof element !== "object") {
    throw structuredError(
      "OP_INVALID",
      "exclusion element must be { target, operator }",
    );
  }
  return {
    target: exclusionTargetToIr(element.target),
    operator: element.operator,
  };
}

function exclusionTargetToIr(target: ExclusionElementArg["target"]): Node {
  if (typeof target === "string") {
    requireString(target, "exclusion target column");
    return { kind: "column", name: target };
  }
  const expr = resolveExpr(target as ExprFn | ExprChainType | Node | undefined);
  if (!expr) {
    throw structuredError("OP_INVALID", "exclusion target must be a column name or expression");
  }
  return {
    kind: "expr",
    expr,
  };
}

function indexElementToIr(element: IndexElementArg): Node {
  if (typeof element === "string") {
    requireString(element, "index element column");
    return { kind: "column", name: element };
  }
  if (element && typeof element === "object" && "kind" in element) {
    if (element.kind === "column") {
      requireString((element as { name?: unknown }).name, "index column element name");
      const order = indexColumnOrderToIr((element as { order?: unknown }).order);
      return order === "desc"
        ? { kind: "column", name: (element as { name: string }).name, order }
        : { kind: "column", name: (element as { name: string }).name };
    }
    if (element.kind === "expr") {
      const expr = resolveExpr((element as { expr?: ExprFn | ExprChainType | Node }).expr);
      if (!expr) {
        throw structuredError("OP_INVALID", "index expr element needs { kind: \"expr\", expr }");
      }
      return { kind: "expr", expr };
    }
  }
  const expr = resolveExpr(element as ExprFn | ExprChainType | Node | undefined);
  if (!expr) {
    throw structuredError("OP_INVALID", "index element must be a column name or expression");
  }
  return { kind: "expr", expr };
}

function indexColumnOrderToIr(order: unknown): "desc" | undefined {
  if (order === undefined || order === "asc") {
    return undefined;
  }
  if (order === "desc") {
    return "desc";
  }
  throw structuredError("OP_INVALID", "index column order must be \"asc\" or \"desc\"");
}

function recordAddExclusion(
  table: string,
  name: string,
  args: ExclusionAddArgs,
): void {
  push(
    compact({
      op: "addConstraint",
      table,
      constraint: exclusionConstraintFromSpec({ ...args, name }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropConstraint(
  table: string,
  name: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "dropConstraint",
      table,
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateIndex(
  table: string,
  name: string,
  args: {
    columns: IndexElementArg[];
    unique?: boolean;
    using?: import("./types.js").IndexMethod;
    where?: ExprFn;
    include?: readonly string[];
    with?: IndexStorageParamsArg;
    only?: boolean;
    ifNotExists?: boolean;
    schema?: string;
  },
): void {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".index(name).add needs { columns: IndexElementArg[] }");
  }
  push(
    compact({
      op: "createIndex",
      table,
      columns: args.columns.map(indexElementToIr),
      name,
      unique: args.unique,
      using: args.using,
      where: resolveExpr(args.where),
      include: indexIncludeToIr(args.include),
      with: indexWithToIr(args.with),
      only: requireOptionalBoolean(args.only, "index only"),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropIndex(
  table: string,
  name: string,
  args: { ifExists?: boolean; concurrently?: boolean; unique?: boolean; schema?: string },
): void {
  push(
    compact({
      op: "dropIndex",
      name,
      table,
      // `unique` drives the destructive/approval gating at apply — preserved here.
      unique: args.unique,
      concurrently: args.concurrently,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function normalizeInsertRows<R extends Row = Row>(
  rows: R | R[] | undefined,
  what: string,
): { columns: string[]; rows: unknown[][] } {
  if (rows === undefined) throw structuredError("OP_INVALID", `${what}: rows is required`);
  const arr = (Array.isArray(rows) ? rows : [rows]) as R[];
  const columns = arr.length > 0 ? Object.keys(arr[0]) : [];
  const firstKeySet = new Set(columns);
  const normalized = arr.map((r) => {
    const keys = Object.keys(r);
    const values: Record<string, unknown> = {};
    for (const key of keys) values[key] = toIrValue((r as Row)[key]);
    return { keys, values };
  });
  for (let i = 0; i < normalized.length; i++) {
    const keys = normalized[i].keys;
    const sameShape =
      keys.length === columns.length && keys.every((key) => firstKeySet.has(key));
    if (!sameShape) {
      throw structuredError(
        "OP_INVALID",
        `${what}: row ${i} has keys [${keys.join(", ")}], expected [${columns.join(", ")}]; ragged insert rows are not allowed`,
      );
    }
  }
  const positional = normalized.map(({ values }) => columns.map((col) => values[col]));
  return { columns, rows: positional };
}

function recordInsert<R extends Row = Row>(table: string, args: InsertArgs<R>): void {
  const normalized = normalizeInsertRows(args.rows, "insert({ rows })");
  push(
    compact({
      op: "insert",
      table,
      columns: normalized.columns,
      rows: normalized.rows,
      onConflict: normalizeOnConflict(args.onConflict),
      schema: args.schema,
    }),
  );
}

function normalizeOnConflict(
  oc: { columns: string[]; doUpdate?: Record<string, unknown> } | undefined | null,
): Node | undefined {
  if (oc === undefined || oc === null) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns } as Node;
  const doUpdate: Record<string, unknown> = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrValue(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate } as Node;
}

function recordUpdate(table: string, args: UpdateArgs): void {
  push(
    compact({
      op: "update",
      table,
      set: resolveSet(args.set),
      where: resolveExpr(args.where),
      batch: args.batch,
      schema: args.schema,
    }),
  );
}

function recordDel(table: string, args: DelArgs): void {
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "del({ where }): where is mandatory (no unfiltered delete)");
  }
  push(
    compact({
      op: "delete",
      table,
      where: resolveExpr(args.where),
      limit: args.limit,
      schema: args.schema,
    }),
  );
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

function recordBackfill(table: string, args: BackfillArgs): void {
  if (args.set === undefined) throw structuredError("OP_INVALID", "backfill({ set }): set is required");
  push(
    compact({
      op: "backfill",
      table,
      cursorColumn: args.cursorColumn || DEFAULT_BACKFILL_CURSOR,
      batchSize: args.batchSize !== undefined ? args.batchSize : DEFAULT_BACKFILL_BATCH,
      set: resolveSet(args.set),
      filter: resolveExpr(args.where),
      name: args.name || `backfill_${table}`,
      schema: args.schema,
    }),
  );
}

type SelectAstBuilder = ViewQueryBuilder & { __selectAst(): SelectAst };

function normalizeTableRef(input: string | TableRef, what: string): TableRef {
  if (typeof input === "string") return { name: input };
  if (!input || typeof input !== "object") {
    throw structuredError("OP_INVALID", `${what} must be a table name string or { name, schema?, alias? }`);
  }
  requireString(input.name, `${what}.name`);
  if (input.schema !== undefined && input.schema !== null) requireString(input.schema, `${what}.schema`);
  if (input.alias !== undefined && input.alias !== null) requireString(input.alias, `${what}.alias`);
  return compact({ name: input.name, schema: input.schema ?? undefined, alias: input.alias ?? undefined }) as TableRef;
}

function viewExpr(slot: ExprFn | ExprChainType | Node): Expr {
  return resolveExpr(slot)! as unknown as Expr;
}

function normalizeSelectItem(item: string | SelectItem | ExprFn | ExprChainType | Expr): SelectItem {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChainImpl) {
    return { kind: "expr", expr: viewExpr(item as ExprFn | ExprChainType) };
  }
  if (item && typeof item === "object") {
    const node = item as Node;
    if (node.node !== undefined) return { kind: "expr", expr: viewExpr(node) };
    if (node.kind === "colRef") {
      requireString(node.name, "select item colRef.name");
      if (node.table !== undefined && node.table !== null) requireString(node.table, "select item colRef.table");
      if (node.alias !== undefined && node.alias !== null) requireString(node.alias, "select item colRef.alias");
      return compact({
        kind: "colRef",
        table: node.table ?? undefined,
        name: node.name,
        alias: node.alias ?? undefined,
      }) as SelectItem;
    }
    if (node.kind === "expr") {
      if (node.alias !== undefined && node.alias !== null) requireString(node.alias, "select item expr.alias");
      return compact({
        kind: "expr",
        expr: viewExpr(node.expr as ExprFn | ExprChainType | Node),
        alias: node.alias ?? undefined,
      }) as SelectItem;
    }
  }
  throw structuredError("OP_INVALID", "select item must be a column name, expression, or SelectItem object");
}

function normalizeOrderDir(dir: unknown, what: string): "asc" | "desc" | undefined {
  if (dir === undefined || dir === null) return undefined;
  if (dir === "asc" || dir === "desc") return dir;
  throw structuredError("OP_INVALID", `${what}.dir must be asc or desc`);
}

function normalizeOrderItem(item: string | OrderItem | ExprFn | ExprChainType | Expr): OrderItem {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChainImpl) {
    return { kind: "expr", expr: viewExpr(item as ExprFn | ExprChainType) };
  }
  if (item && typeof item === "object") {
    const node = item as Node;
    if (node.node !== undefined) return { kind: "expr", expr: viewExpr(node) };
    if (node.kind === "colRef") {
      requireString(node.name, "order item colRef.name");
      if (node.table !== undefined && node.table !== null) requireString(node.table, "order item colRef.table");
      return compact({
        kind: "colRef",
        table: node.table ?? undefined,
        name: node.name,
        dir: normalizeOrderDir(node.dir, "order item colRef"),
      }) as OrderItem;
    }
    if (node.kind === "expr") {
      return compact({
        kind: "expr",
        expr: viewExpr(node.expr as ExprFn | ExprChainType | Node),
        dir: normalizeOrderDir(node.dir, "order item expr"),
      }) as OrderItem;
    }
  }
  throw structuredError("OP_INVALID", "orderBy item must be a column name, expression, or OrderItem object");
}

function viewQueryBuilder(): SelectAstBuilder {
  const state: {
    from?: TableRef;
    projection: SelectItem[];
    joins: Join[];
    where?: Expr;
    orderBy?: OrderItem[];
    limit?: number;
  } = {
    projection: [],
    joins: [],
  };

  let builder: SelectAstBuilder;
  builder = {
    from(table: string | TableRef) {
      state.from = normalizeTableRef(table, "view query from(table)");
      return builder;
    },
    select(items: Array<string | SelectItem | ExprFn | ExprChainType | Expr>) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query select(items): items must be an array");
      }
      state.projection = items.map(normalizeSelectItem);
      return builder;
    },
    join(kind: JoinKind, table: string | TableRef, on: ExprFn | ExprChainType | Expr) {
      if (kind !== "inner" && kind !== "left") {
        throw structuredError("OP_INVALID", "view query join(kind): kind must be inner or left");
      }
      state.joins.push({
        kind,
        table: normalizeTableRef(table, "view query join(table)"),
        on: viewExpr(on as ExprFn | ExprChainType | Node),
      });
      return builder;
    },
    innerJoin(table: string | TableRef, on: ExprFn | ExprChainType | Expr) {
      return builder.join("inner", table, on);
    },
    leftJoin(table: string | TableRef, on: ExprFn | ExprChainType | Expr) {
      return builder.join("left", table, on);
    },
    where(expr: ExprFn | ExprChainType | Expr) {
      state.where = viewExpr(expr as ExprFn | ExprChainType | Node);
      return builder;
    },
    orderBy(items: Array<string | OrderItem | ExprFn | ExprChainType | Expr>) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query orderBy(items): items must be an array");
      }
      state.orderBy = items.map(normalizeOrderItem);
      return builder;
    },
    limit(n: number) {
      if (typeof n !== "number" || !Number.isInteger(n) || n < 0) {
        throw structuredError("OP_INVALID", `view query limit(n): n must be a non-negative integer, got ${n}`);
      }
      state.limit = n;
      return builder;
    },
    __selectAst() {
      if (state.from === undefined) {
        throw structuredError("OP_INVALID", "view query must call q.from(table)");
      }
      return compact({
        from: state.from,
        projection: state.projection,
        joins: state.joins.length ? state.joins : undefined,
        where: state.where,
        orderBy: state.orderBy,
        limit: state.limit,
      }) as SelectAst;
    },
  };

  return builder;
}

function isSelectAstBuilder(x: unknown): x is SelectAstBuilder {
  return Boolean(x && typeof x === "object" && typeof (x as SelectAstBuilder).__selectAst === "function");
}

function isSelectAst(x: unknown): x is SelectAst {
  return Boolean(x && typeof x === "object" && (x as SelectAst).from !== undefined);
}

function resolveSelectAst(as: CreateViewArgs["as"]): SelectAst {
  if (typeof as === "function") {
    const q = viewQueryBuilder();
    const built = as(q) || q;
    if (isSelectAstBuilder(built)) return built.__selectAst();
    if (isSelectAst(built)) return built;
  }
  if (isSelectAstBuilder(as)) return as.__selectAst();
  if (isSelectAst(as)) return as;
  throw structuredError("OP_INVALID", "view.create({ as }) must be a query-builder callback or SelectAst");
}

function recordCreateView(name: string, args: CreateViewArgs & { schema?: string }): void {
  if (!args || args.as === undefined) {
    throw structuredError("OP_INVALID", "view(name).create({ as }) requires a structured SelectAst builder");
  }
  push(
    compact({
      op: "createView",
      name,
      schema: args.schema,
      columns: args.columns,
      query: { kind: "structured", select: resolveSelectAst(args.as) },
      replace: args.replace,
      materialized: args.materialized,
    }),
  );
}

function recordCreateRawView(name: string, args: CreateRawViewArgs & { schema?: string }): void {
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "view(name).createRaw({ sql }) needs an object");
  }
  requireString(args.sql, "view(name).createRaw({ sql })");
  push(
    compact({
      op: "createView",
      name,
      schema: args.schema,
      columns: args.columns,
      query: { kind: "raw", sql: args.sql },
      replace: args.replace,
      materialized: args.materialized,
    }),
  );
}

function recordDropView(name: string, args: DropViewArgs & { schema?: string }): void {
  push(
    compact({
      op: "dropView",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
      materialized: args.materialized,
    }),
  );
}

const TRIGGER_RAISE_LEVELS = ["abort", "fail", "ignore", "rollback"] as const;

function triggerBodyBuilder(): TriggerBodyBuilder {
  return {
    raise(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.raise({ level, message, errcode? }) needs an object");
      }
      requireString(args.level, "b.raise({ level })");
      if (!(TRIGGER_RAISE_LEVELS as readonly string[]).includes(args.level)) {
        throw structuredError(
          "OP_INVALID",
          `b.raise({ level }): level must be one of ${TRIGGER_RAISE_LEVELS.join(" | ")}, ` +
            `got ${JSON.stringify(args.level)}`,
          { level: args.level },
        );
      }
      requireString(args.message, "b.raise({ message })");
      if (args.errcode !== undefined) requireString(args.errcode, "b.raise({ errcode })");
      return compact({
        stmt: "raise",
        level: args.level,
        message: args.message,
        errcode: args.errcode,
      }) as TriggerStmt;
    },
    insert(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.insert({ table, rows, schema? }) needs an object");
      }
      requireString(args.table, "b.insert({ table })");
      const normalized = normalizeInsertRows(args.rows, "b.insert({ rows })");
      return compact({
        stmt: "insert",
        table: args.table,
        columns: normalized.columns,
        rows: normalized.rows,
        schema: args.schema,
      }) as TriggerStmt;
    },
    update(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.update({ table, set, where?, schema? }) needs an object");
      }
      requireString(args.table, "b.update({ table })");
      return compact({
        stmt: "update",
        table: args.table,
        set: resolveSet(args.set),
        where: resolveExpr(args.where),
        schema: args.schema,
      }) as TriggerStmt;
    },
    del(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.del({ table, where, limit?, schema? }) needs an object");
      }
      requireString(args.table, "b.del({ table })");
      if (args.where === undefined || args.where === null) {
        throw structuredError("OP_INVALID", "b.del({ where }): where is mandatory (no unfiltered delete)");
      }
      return compact({
        stmt: "delete",
        table: args.table,
        where: resolveExpr(args.where),
        limit: args.limit,
        schema: args.schema,
      }) as TriggerStmt;
    },
    select(expr) {
      return { stmt: "select", expr: resolveExpr(expr)! } as TriggerStmt;
    },
  };
}

function resolveTriggerAction(args: CreateTriggerArgs): Node {
  const hasExecute = "execute" in args && args.execute !== undefined;
  const hasBody = "body" in args && args.body !== undefined;
  if (hasExecute === hasBody) {
    throw structuredError(
      "OP_INVALID",
      ".createTrigger(...) needs exactly one action: { execute: string } or { body: (b) => TriggerStmt[] }",
    );
  }
  if (hasExecute) {
    requireString(args.execute, ".createTrigger({ execute })");
    return { kind: "executeFunction", name: args.execute };
  }
  if (!("body" in args) || typeof args.body !== "function") {
    throw structuredError("OP_INVALID", ".createTrigger({ body }) must be a function");
  }
  const statements = args.body(triggerBodyBuilder());
  if (!Array.isArray(statements)) {
    throw structuredError("OP_INVALID", ".createTrigger({ body }) must return an array of trigger statements");
  }
  for (const stmt of statements) {
    if (!stmt || typeof stmt !== "object" || typeof (stmt as Node).stmt !== "string") {
      throw structuredError("OP_INVALID", "trigger body entries must be statements returned by the trigger body builder");
    }
  }
  return { kind: "body", statements };
}

// ── (E) The fluent `table()` handle — the reusable table entry (§3) ──

/** Per-op-wins-over-table-default schema precedence (§3/§4): a per-op `schema`
 *  overrides the table default only when the KEY is present with a defined value;
 *  an omitted key (or an explicit `undefined`) keeps the table default. */
function pickSchema(perCall: { schema?: string } | undefined, dflt: string | undefined): string | undefined {
  if (perCall && perCall.schema !== undefined) return perCall.schema;
  return dflt;
}

function pickViewColumns(
  perCall: { columns?: string[] } | undefined,
  dflt: string[] | undefined,
): string[] | undefined {
  if (perCall && perCall.columns !== undefined) return perCall.columns;
  return dflt;
}

function requireColumnDef(x: unknown, where: string): asserts x is ColumnDefImpl {
  if (!isColumnDef(x)) {
    throw structuredError("OP_INVALID", `${where} must be a t.* ColumnDef`);
  }
}

export function comment(target: CommentTargetArg, text: string | null): void {
  recordComment(target, text);
}

export function partition(name: string, opts: PartitionOptions = {}): PartitionHandle {
  requireString(name, "partition(name, …)");
  const dflt = opts.schema;

  return {
    of(parent) {
      requireString(parent, "partition(name).of(parent)");
      return {
        forValues(bounds, args = {}) {
          recordCreatePartition(name, parent, partitionBoundsFromForValues(bounds), {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt),
          });
        },
        asDefault(args = {}) {
          recordCreatePartition(name, parent, { kind: "default" }, {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt),
          });
        },
      };
    },
  };
}

export function dropPartition(name: string, args: { ifExists?: boolean; cascade?: boolean; schema?: string } = {}): void {
  requireString(name, "dropPartition(name)");
  recordDropPartition(name, {
    ifExists: args.ifExists,
    cascade: args.cascade,
    schema: args.schema,
  });
}

export function table(name: string, opts: TableOptions = {}): TableHandle {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;

  const handle: TableHandle = {
    // §3.1 — the table itself
    create(args) {
      recordCreateTable(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    drop(args = {}) {
      recordDropTable(name, {
        ifExists: args.ifExists,
        cascade: args.cascade,
        schema: pickSchema(args, dflt),
      });
      return handle;
    },
    rename(args) {
      requireString(args.to, "table(name).rename({ to })");
      recordRenameTable(name, args.to, {
        ifExists: args.ifExists,
        schema: pickSchema(args, dflt),
      });
      return handle;
    },
    setOptions(args) {
      recordSetTableOptions(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    softDelete(enabled = true, args = {}) {
      recordSetTableOptions(name, { softDelete: enabled, schema: pickSchema(args, dflt) });
      return handle;
    },
    withVersioning(enabled = true, args = {}) {
      recordSetTableOptions(name, { versioning: enabled, schema: pickSchema(args, dflt) });
      return handle;
    },
    strictness(level, args = {}) {
      recordSetTableOptions(name, { strictness: level, schema: pickSchema(args, dflt) });
      return handle;
    },
    comment(text, args = {}) {
      recordComment({ kind: "table", name, schema: pickSchema(args, dflt) }, text);
      return handle;
    },
    detachPartition(partitionName, args = {}) {
      requireString(partitionName, ".detachPartition(name)");
      recordDetachPartition(name, partitionName, {
        concurrently: args.concurrently,
        schema: pickSchema(args, dflt),
      });
      return handle;
    },
    // §3.2 — columns
    column(col): ColumnRef {
      requireString(col, ".column(name)");
      const id = registerSelector("column", col);
      return {
        add(args) {
          requireColumnDef(args.type, ".column(name).add({ type })");
          terminateSelector(id);
          recordAddColumn(name, col, args.type, {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropColumn(name, col, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        },
        rename(args) {
          requireString(args.to, ".column(name).rename({ to })");
          requireColumnDef(args.type, ".column(name).rename({ type })");
          terminateSelector(id);
          recordRenameColumn(name, col, args.to, args.type, { schema: pickSchema(args, dflt) });
          return handle;
        },
        setType(args) {
          terminateSelector(id);
          recordSetColumnType(name, col, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
        setNotNull(args = {}) {
          terminateSelector(id);
          recordSetColumnNotNull(name, col, { schema: pickSchema(args, dflt) });
          return handle;
        },
        dropNotNull(args = {}) {
          terminateSelector(id);
          recordDropColumnNotNull(name, col, { schema: pickSchema(args, dflt) });
          return handle;
        },
        setDefault(value, args = {}) {
          terminateSelector(id);
          recordSetColumnDefault(name, col, value, { schema: pickSchema(args, dflt) });
          return handle;
        },
        dropDefault(args = {}) {
          terminateSelector(id);
          recordDropColumnDefault(name, col, { schema: pickSchema(args, dflt) });
          return handle;
        },
        comment(text, args = {}) {
          terminateSelector(id);
          recordComment({ kind: "column", table: name, name: col, schema: pickSchema(args, dflt) }, text);
          return handle;
        },
      };
    },

    // §3.3 — constraints
    foreignKey(fkName): ForeignKeyRef {
      requireString(fkName, ".foreignKey(name)");
      const id = registerSelector("foreignKey", fkName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddForeignKey(name, fkName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    addForeignKey(fkName, args) {
      requireString(fkName, ".addForeignKey(name, args)");
      recordAddForeignKey(name, fkName, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    unique(uqName): UniqueRef {
      requireString(uqName, ".unique(name)");
      const id = registerSelector("unique", uqName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddUnique(name, uqName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    check(ckName): CheckRef {
      requireString(ckName, ".check(name)");
      const id = registerSelector("check", ckName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddCheck(name, ckName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    addCheck(ckName, expr, args = {}) {
      requireString(ckName, ".addCheck(name, expr)");
      recordAddCheck(name, ckName, { expr, ifNotExists: args.ifNotExists, schema: pickSchema(args, dflt) });
      return handle;
    },
    exclusion(exName): ExclusionRef {
      requireString(exName, ".exclusion(name)");
      const id = registerSelector("exclusion", exName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddExclusion(name, exName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },
    constraint(cName): ConstraintRef {
      requireString(cName, ".constraint(name)");
      const id = registerSelector("constraint", cName);
      return {
        drop(args = {}) {
          terminateSelector(id);
          recordDropConstraint(name, cName, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        },
        comment(text, args = {}) {
          terminateSelector(id);
          recordComment({ kind: "constraint", table: name, name: cName, schema: pickSchema(args, dflt) }, text);
          return handle;
        },
      };
    },

    // §3.4 — indexes
    index(idxName): IndexRef {
      requireString(idxName, ".index(name)");
      const id = registerSelector("index", idxName);
      const indexDraft: {
        using?: import("./types.js").IndexMethod;
        include?: readonly string[];
        with?: IndexStorageParamsArg;
        only?: boolean;
      } = {};
      const indexRef: IndexRef = {
        using(method) {
          indexDraft.using = method;
          return indexRef;
        },
        include(columns) {
          indexDraft.include = columns;
          return indexRef;
        },
        with(params) {
          indexDraft.with = params;
          return indexRef;
        },
        only(enabled = true) {
          indexDraft.only = enabled;
          return indexRef;
        },
        add(args) {
          terminateSelector(id);
          recordCreateIndex(name, idxName, { ...indexDraft, ...args, schema: pickSchema(args, dflt) });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropIndex(name, idxName, {
            ifExists: args.ifExists,
            concurrently: args.concurrently,
            unique: args.unique,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        comment(text, args = {}) {
          terminateSelector(id);
          recordComment({ kind: "index", name: idxName, schema: pickSchema(args, dflt) }, text);
          return handle;
        },
      };
      return indexRef;
    },

    // §3.5 — table data (no existence guard; schema rides on args)
    insert(args) {
      recordInsert(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    update(args) {
      recordUpdate(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    del(args) {
      recordDel(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    backfill(args) {
      recordBackfill(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },

    // `@zeroship/migrate/pg` — table-scoped privileged primitives.
    enableRowLevelSecurity() {
      push(compact({ op: "enableRls", table: name, schema: dflt }));
      return handle;
    },
    forceRowLevelSecurity() {
      push(compact({ op: "forceRls", table: name, schema: dflt }));
      return handle;
    },
    disableRowLevelSecurity() {
      push(compact({ op: "disableRls", table: name, schema: dflt }));
      return handle;
    },
    noForceRowLevelSecurity() {
      push(compact({ op: "noForceRls", table: name, schema: dflt }));
      return handle;
    },
    createPolicy(args: CreateTablePolicyArgs) {
      requireString(args.name, ".createPolicy({ name })");
      if (Array.isArray(args.to) && args.to.length === 0) {
        throw structuredError("OP_INVALID", ".createPolicy({ to }): to must be a non-empty role array (omit to for PUBLIC)");
      }
      if (args.using === undefined) {
        throw structuredError("OP_INVALID", ".createPolicy({ using }): using is required (the renderer always emits USING)");
      }
      push(compact({
        op: "createPolicy",
        name: args.name,
        table: name,
        schema: pickSchema(args, dflt),
        forCmd: args.for || "all",
        to: args.to,
        using: resolveExpr(args.using),
        withCheck: resolveExpr(args.withCheck),
      }));
      return handle;
    },
    dropPolicy(args: DropTablePolicyArgs) {
      requireString(args.name, ".dropPolicy({ name })");
      push(compact({
        op: "dropPolicy",
        name: args.name,
        table: name,
        schema: pickSchema(args, dflt),
        ifExists: args.ifExists,
      }));
      return handle;
    },
    createTrigger(args) {
      requireString(args.name, ".createTrigger({ name })");
      push(compact({
        op: "createTrigger",
        name: args.name,
        table: name,
        schema: pickSchema(args, dflt),
        timing: args.timing,
        events: args.events,
        forEach: args.forEach,
        action: resolveTriggerAction(args),
        when: resolveExpr(args.when),
      }));
      return handle;
    },
    dropTrigger(args) {
      requireString(args.name, ".dropTrigger({ name })");
      push(compact({
        op: "dropTrigger",
        name: args.name,
        table: name,
        schema: pickSchema(args, dflt),
        ifExists: args.ifExists,
      }));
      return handle;
    },
  };

  return handle;
}

export function view(name: string, opts: ViewOptions = {}): ViewHandle {
  requireString(name, "view(name, …)");
  const dflt = opts.schema;
  const dfltColumns = opts.columns;

  const handle: ViewHandle = {
    create(args) {
      recordCreateView(name, {
        ...args,
        schema: pickSchema(args, dflt),
        columns: pickViewColumns(args, dfltColumns),
      });
      return handle;
    },
    createRaw(args) {
      recordCreateRawView(name, {
        ...args,
        schema: pickSchema(args, dflt),
        columns: pickViewColumns(args, dfltColumns),
      });
      return handle;
    },
    drop(args = {}) {
      recordDropView(name, {
        ifExists: args.ifExists,
        materialized: args.materialized,
        schema: pickSchema(args, dflt),
      });
      return handle;
    },
    comment(text, args = {}) {
      recordComment({ kind: "view", name, schema: pickSchema(args, dflt) }, text);
      return handle;
    },
  };

  return handle;
}

// ── (C) Determinism lint ──

const NONDETERMINISM_PATTERNS: { re: RegExp; name: string; steer: string }[] = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "the Date.now symbol (no parens) or c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "the Math.random symbol (no parens) or c.fn.genRandomUuid()" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "the crypto.randomUUID symbol (no parens) or c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "the Date.now symbol (no parens) or c.fn.now()" },
];

/**
 * Lint a migration's SOURCE for the nondeterminism accessors (`Date.now()` /
 * `Math.random()` / `crypto.randomUUID()` / `new Date()`).
 *
 * SCOPE — intentional coarse whole-source scan: it OVER-flags (a clock accessor
 * in a comment trips it) and NEVER under-flags. The build-once committed artifact
 * already neutralizes post-deploy non-determinism, so the lint's only job is a
 * best-effort pre-commit STEER where a false positive is cheap (rephrase) and a
 * false negative (a baked build-time value slipping through) is the real hazard.
 * Findings are surfaced as WARNINGS on the record path, never a hard reject.
 */
export function lintDeterminism(source: string): DeterminismFinding[] {
  if (typeof source !== "string") return [];
  const findings: DeterminismFinding[] = [];
  for (const { re, name, steer } of NONDETERMINISM_PATTERNS) {
    if (re.test(source)) {
      findings.push({
        code: "NONDETERMINISTIC_OP_ARG",
        accessor: name,
        suggested_fix: `replace ${name} with the DB-evaluated ${steer}`,
        reason: `${name} bakes a build-time value into the artifact; use the structured FnSynth scalar`,
      });
    }
  }
  return findings;
}

function splitPartGrammarLint(delim: unknown, n: unknown): void {
  const fail = (reason: string) => {
    throw structuredError("EXPR_NOT_PORTABLE", reason, {
      suggested_fix:
        "pass a non-empty string-literal delimiter and a positive-integer n; to target " +
        "SQLite too, stay in-envelope (single-ASCII delimiter, 1<=n<=8); out-of-envelope " +
        "forms are only renderable on dialects with a native renderer such as Postgres/MySQL",
    });
  };
  if (typeof delim !== "string") fail(`c.fn.splitPart delimiter must be a string literal; got ${typeof delim}`);
  if ((delim as string).length === 0) fail("c.fn.splitPart delimiter must be a non-empty string literal");
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`c.fn.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if ((n as number) < 1) fail(`c.fn.splitPart part index n must be a positive integer; got ${n}`);
}
