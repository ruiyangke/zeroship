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
  AggNamespace,
  BackfillArgs,
  CheckBuilderWithPg,
  CheckDef,
  CheckExprFn,
  ColType,
  ColumnDef as ColumnDefType,
  ColumnRef,
  CommentTargetArg,
  AlterSequenceArgs,
  CastTarget,
  CreateDomainArgs,
  CreateEnumArgs,
  CreateSequenceArgs,
  CurrentSettingOptions,
  CreateTableArgs,
  TriggerCreateArgs,
  CreateViewArgs,
  DbSynthSymbol,
  DecimalValue,
  BytesValue,
  DefaultBuilder,
  DefaultExprFn,
  DefaultValue,
  DelArgs,
  DmlSetValue,
  DomainCheckFn,
  DomainHandle,
  DomainValueBuilder,
  DeterminismFinding,
  DropDomainArgs,
  DropEnumArgs,
  DropSequenceArgs,
  DropViewArgs,
  Duration,
  DroppedExtensionHandle,
  DroppedRoleHandle,
  DroppedSchemaHandle,
  ExtensionCreateArgs,
  ExtensionDropArgs,
  ExtensionHandle,
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
  GeneratedColumnExprFn,
  GeneratedOptions,
  IdOptions,
  IdentityOptions,
  ImmutableFnNamespace,
  IndexExprBuilder,
  IndexExprFn,
  IndexRef,
  IndexElementArg,
  InsertArgs,
  IndexStorageParamsArg,
  Join,
  JoinKind,
  MaskOptions,
  NextvalDefault,
  NextvalOptions,
  OrderItem,
  PartitionBoundArgs,
  PartitionBoundInput,
  PartitionBoundSentinel,
  PartitionByInput,
  PgCheckExprFn,
  PgConstraintRef,
  PgCheckRef,
  PgExprNamespace,
  PgIndexAdd,
  PgIndexDropArgs,
  PgIndexRef,
  PgTableHandle,
  RoleCreateArgs,
  RoleDropArgs,
  RoleHandle,
  RoleSetOptionsArgs,
  RefAction,
  Row,
  Scalar,
  ScalarValue,
  SchemaCreateArgs,
  SchemaDropArgs,
  SchemaHandle,
  SelectAst,
  SelectItem,
  SequenceHandle,
  TableHandle,
  TableOptions,
  TableRuntimeOptions,
  TableStrictness,
  TableRef,
  TextOptions,
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

// Capture the native nondeterministic function symbols before a migration module
// can mutate globals. A bare symbol (`crypto.randomUUID` without parens) is an
// opt-in to DB-side evaluation, matched by IDENTITY below. In the constrained
// engine V8 recorder isolate (the `FrontendGlobals::Migration` profile installs
// no Web Crypto), `globalThis.crypto` may be absent, so a bare `crypto.randomUUID`
// in the migration source would be a `ReferenceError`; install an identity-only
// stub so the symbol resolves (its `randomUUID` throws if CALLED — the symbol form
// is record-time-only). GUARDED by `if absent`: in Node / browsers, where real Web
// Crypto exists, both guards skip and this is a pure no-op (the real
// `crypto.randomUUID` is captured). This ran at the top of the former
// `migrate_ops.js` twin; it moves here so the one compiled recorder artifact keeps
// the identical engine behavior. MUST precede the capture below.
if (typeof globalThis !== "undefined") {
  if (typeof globalThis.crypto === "undefined" || globalThis.crypto === null) {
    Object.defineProperty(globalThis, "crypto", {
      value: {},
      configurable: true,
      writable: false,
    });
  }
  if (typeof globalThis.crypto.randomUUID !== "function") {
    Object.defineProperty(globalThis.crypto, "randomUUID", {
      value: function randomUUID() {
        throw new Error(
          "crypto.randomUUID() is not available in the migration recorder; " +
            "use the crypto.randomUUID symbol (no parens) or genRandomUuid()",
        );
      },
      configurable: true,
      writable: false,
    });
  }
}

const nativeDateNow = typeof Date !== "undefined" ? Date.now : undefined;
const nativeMathRandom = typeof Math !== "undefined" ? Math.random : undefined;
const nativeCryptoRandomUUID =
  typeof globalThis.crypto !== "undefined" && typeof globalThis.crypto.randomUUID === "function"
    ? globalThis.crypto.randomUUID
    : undefined;
const NEXTVAL_DEFAULT_MARKER = "__zeroshipMigrateNextvalDefault";
const DECIMAL_VALUE_BRAND = Symbol.for("zeroship.migrate.decimal/v1");
const BYTES_VALUE_BRAND = Symbol.for("zeroship.migrate.bytes/v1");
const DECIMAL_STRING_RE = /^-?\d+(?:\.\d+)?$/;
const BASE64_STRING_RE = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}(?:==)?|[A-Za-z0-9+/]{3}=?)?$/;
const DECIMAL_VALUE_ERROR =
  'decimal(value) requires a well-formed decimal string; use decimal("<n>") or decimal("0.00")';
const BYTES_VALUE_ERROR =
  'byteValue(bytes) requires a Uint8Array or well-formed base64 string; use byteValue(new Uint8Array([...])) or byteValue("<base64>")';

function requireDecimalString(value: unknown): string {
  if (typeof value !== "string" || !DECIMAL_STRING_RE.test(value)) {
    throw structuredError("OP_INVALID", DECIMAL_VALUE_ERROR);
  }
  return value;
}

export function decimal(value: string): DecimalValue {
  return Object.freeze({
    [DECIMAL_VALUE_BRAND]: true,
    decimal: requireDecimalString(value),
  }) as unknown as DecimalValue;
}

function requireBase64String(value: unknown): string {
  if (typeof value !== "string" || !BASE64_STRING_RE.test(value)) {
    throw structuredError("OP_INVALID", BYTES_VALUE_ERROR);
  }
  const padded = value + "=".repeat((4 - (value.length % 4)) % 4);
  try {
    const normalized = btoa(atob(padded));
    if (normalized !== padded) throw new Error("non-canonical base64");
    return normalized;
  } catch {
    throw structuredError("OP_INVALID", BYTES_VALUE_ERROR);
  }
}

export function byteValue(bytes: Uint8Array | string): BytesValue {
  return Object.freeze({
    [BYTES_VALUE_BRAND]: true,
    bytes: bytes instanceof Uint8Array ? bytesToBase64(bytes) : requireBase64String(bytes),
  }) as unknown as BytesValue;
}

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

// ── The `defineOp` chokepoint + derived tier-1 producer registry (S0.3) ──
//
// Every op producer emits through an emitter minted by `defineOp(kind, …)`, the
// single chokepoint wrapping `push`/`pushOrDeferUp`. The emitter is
// behaviour-preserving: it records `compact({ op: kind, ...payload })` —
// byte-identical to the prior in-line `push(compact({ op: kind, … }))`. Each mint
// APPENDS to `tier1Producers`, so the producer registry (op-kind → producer(s)) is
// DERIVED from the mints, never self-reported. A later slice's census asserts
// one-producer-per-op-kind over this data (which is why the multi-producer kind
// `addConstraint` mints several distinct producers here). Mirrored 1:1
// in the engine twin `migrate_ops.js`.

/** One tier-1 op-emission producer, DERIVED from a `defineOp` mint call. */
export interface OpProducer {
  /** The op discriminant this producer stamps as the node's `op` field. */
  readonly kind: string;
  /** A stable identity for the emission site (census grouping + diagnostics). */
  readonly producer: string;
  /** Whether the emitter defers into the up-phase buffer (`pushOrDeferUp`). */
  readonly deferrable: boolean;
}

const tier1Producers: OpProducer[] = [];

type OpEmitter = (payload: Node) => Node;

/** Mint the single emitter every producer for `kind` records through, and register
 *  the (kind → producer) fact so the tier-1 registry is derivable. `producer`
 *  defaults to `kind` (the common one-producer-per-kind case). */
function defineOp(kind: string, producer: string = kind, opts: { deferrable?: boolean } = {}): OpEmitter {
  const deferrable = opts.deferrable === true;
  tier1Producers.push({ kind, producer, deferrable });
  const sink = deferrable ? pushOrDeferUp : push;
  return (payload: Node): Node => sink(compact({ op: kind, ...payload }));
}

/** The flat tier-1 producer list, in mint (declaration) order — DATA the census
 *  consumes (a later slice asserts one-producer-per-op-kind over it). */
export function opProducers(): readonly OpProducer[] {
  return tier1Producers;
}

/** The tier-1 producer registry: per op-kind, the producer(s) that emit it —
 *  DERIVED from the `defineOp` mints (never self-reported). */
export function opProducerRegistry(): ReadonlyMap<string, readonly OpProducer[]> {
  const byKind = new Map<string, OpProducer[]>();
  for (const producer of tier1Producers) {
    const list = byKind.get(producer.kind);
    if (list === undefined) byKind.set(producer.kind, [producer]);
    else list.push(producer);
  }
  return byKind;
}

// The minted emitters — one per producer site. Multi-producer op-kinds
// (`addConstraint`) mint several, so the registry surfaces the duplication as
// data.
const emitCreateEnum = defineOp("createEnum", "createEnum", { deferrable: true });
const emitDropEnum = defineOp("dropEnum");
const emitCreateDomain = defineOp("createDomain", "createDomain", { deferrable: true });
const emitDropDomain = defineOp("dropDomain");
const emitCreateSequence = defineOp("createSequence", "createSequence", { deferrable: true });
const emitAlterSequence = defineOp("alterSequence");
const emitDropSequence = defineOp("dropSequence");
const emitCreateSchema = defineOp("createSchema");
const emitDropSchema = defineOp("dropSchema");
const emitCreateExtension = defineOp("createExtension");
const emitDropExtension = defineOp("dropExtension");
const emitCreateRole = defineOp("createRole");
const emitAlterRole = defineOp("alterRole");
const emitDropRole = defineOp("dropRole");
const emitComment = defineOp("comment");
const emitCreateTable = defineOp("createTable");
const emitCreatePartition = defineOp("createPartition");
const emitAttachPartition = defineOp("attachPartition");
const emitDetachPartition = defineOp("detachPartition");
const emitDropPartition = defineOp("dropPartition");
const emitSetTableOptions = defineOp("setTableOptions");
const emitDropTable = defineOp("dropTable");
const emitRenameTable = defineOp("renameTable");
const emitAddColumn = defineOp("addColumn");
const emitAddColumnUnique = defineOp("addConstraint", "addColumn.unique");
const emitDropColumn = defineOp("dropColumn");
const emitRenameColumn = defineOp("renameColumn");
const emitSetColumnType = defineOp("setColumnType");
const emitSetColumnNotNull = defineOp("setColumnNotNull");
const emitDropColumnNotNull = defineOp("dropColumnNotNull");
const emitSetColumnDefault = defineOp("setColumnDefault");
const emitDropColumnDefault = defineOp("dropColumnDefault");
const emitAddForeignKey = defineOp("addConstraint", "foreignKey");
const emitAddUnique = defineOp("addConstraint", "unique");
const emitAddCheck = defineOp("addConstraint", "check");
const emitAddExclusion = defineOp("addConstraint", "exclusion");
const emitDropConstraint = defineOp("dropConstraint");
const emitValidateConstraint = defineOp("validateConstraint");
const emitCreateIndex = defineOp("createIndex");
const emitDropIndex = defineOp("dropIndex");
const emitInsert = defineOp("insert");
const emitUpdate = defineOp("update");
const emitDelete = defineOp("delete");
const emitBackfill = defineOp("backfill");
const emitCreateView = defineOp("createView", "view.create");
const emitDropView = defineOp("dropView");
const emitSetRls = defineOp("setRls");
const emitCreatePolicy = defineOp("createPolicy");
const emitDropPolicy = defineOp("dropPolicy");
const emitCreateTrigger = defineOp("createTrigger");
const emitDropTrigger = defineOp("dropTrigger");

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

function requirePlainObject(v: unknown, what: string): asserts v is Record<string, unknown> {
  if (v === null || typeof v !== "object" || Array.isArray(v)) {
    throw structuredError("OP_INVALID", `${what} must be an object`);
  }
}

function requireOptionalPositiveInteger(v: unknown, what: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isInteger(v) || v <= 0) {
    throw structuredError("OP_INVALID", `${what} must be a positive integer; got ${v}`);
  }
  return v;
}

function requireOptionalNonNegativeInteger(v: unknown, what: string): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isInteger(v) || v < 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-negative integer; got ${v}`);
  }
  return v;
}

function indexElementFacet(v: unknown, what: string): string | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "string" || v.length === 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-empty string`);
  }
  return v;
}

function runtimeOptionsFromCreateArgs(args: CreateTableArgs): Node | undefined {
  const opts = args.options;
  if (opts === undefined) return undefined;
  requirePlainObject(opts, "create({ options })");
  const softDelete = requireOptionalBoolean(opts.softDelete, "create({ options: { softDelete } })");
  const versioning = requireOptionalBoolean(opts.versioning, "create({ options: { versioning } })");
  const strictness = requireStrictness(opts.strictness, "create({ options: { strictness } })");
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

function runtimeOptionsPatchFromArgs(args: TableRuntimeOptions): Node {
  requirePlainObject(args, "setOptions(args)");
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
 *  `migrate_ops.js`) so `t.vector({ dimensions, metric })` rejects an out-of-set metric
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
  // (`t.vector({ dimensions, metric })`). #174: a standalone column mask (`.mask({…})`).
  // Absent ⇒ omitted on the wire.
  readonly _idPrefix: string | undefined;
  readonly _vectorMetric: string | undefined;
  readonly _caseSensitive: boolean | undefined;
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
      caseSensitive?: boolean;
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
    this._caseSensitive = fields?.caseSensitive;
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
    caseSensitive?: boolean;
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
      caseSensitive: "caseSensitive" in over ? over.caseSensitive : this._caseSensitive,
      mask: "mask" in over ? over.mask : this._mask,
      generated: "generated" in over ? over.generated : this._generated,
      identity: "identity" in over ? over.identity : this._identity,
    });
  }

  /** Internal: carry the typed-id prefix (`t.id({prefix})`). */
  __withIdPrefix(prefix: string): ColumnDefImpl {
    return this.with({ idPrefix: prefix });
  }
  /** Internal: carry the pgvector distance metric (`t.vector({ dimensions, metric })`). */
  __withVectorMetric(metric: string): ColumnDefImpl {
    return this.with({ vectorMetric: metric });
  }

  notNull(): ColumnDefImpl {
    return this.with({ nullable: false });
  }
  default(value: DefaultValue | DefaultExprFn | ExprChainType | Expr): ColumnDefImpl {
    return this.with({ default: toIrDefault(value) });
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
   *  `MASK_CLASSIFICATIONS`). The closed-set checks mirror `t.vector({ dimensions, metric })`:
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

  generated(expr: GeneratedColumnExprFn | ExprChainType | Expr, opts?: GeneratedOptions): ColumnDefImpl {
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.generated(expr, opts): opts must be { virtual?: boolean }");
    }
    if (opts?.virtual !== undefined && typeof opts.virtual !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.generated(expr, { virtual }): virtual must be a boolean");
    }
    return this.with({
      generated: {
        expr: resolveImmutableExpr(expr as GeneratedColumnExprFn | ExprChainType | Node, "generated column expression")!,
        stored: opts?.virtual === true ? false : true,
      },
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

  autoIncrement(): ColumnDefImpl {
    return this.with({ identity: { always: false } });
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
      caseSensitive: this._caseSensitive === false ? false : undefined,
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
      caseSensitive: this._caseSensitive === false ? false : undefined,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity,
    });
  }
}

function isColumnDef(x: unknown): x is ColumnDefImpl {
  return x instanceof ColumnDefImpl;
}

function textColumn(opts?: TextOptions): ColumnDefImpl {
  if (opts !== undefined && (opts === null || typeof opts !== "object")) {
    throw structuredError("OP_INVALID", "t.text(opts): opts must be { caseSensitive?: boolean }");
  }
  if (opts?.caseSensitive !== undefined && typeof opts.caseSensitive !== "boolean") {
    throw structuredError("OP_INVALID", "t.text({ caseSensitive }): caseSensitive must be a boolean");
  }
  return new ColumnDefImpl("text", {
    caseSensitive: opts?.caseSensitive === false ? false : undefined,
  });
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

function isDecimalValue(value: unknown): value is DecimalValue {
  return (
    value !== null &&
    typeof value === "object" &&
    (value as Record<PropertyKey, unknown>)[DECIMAL_VALUE_BRAND] === true &&
    typeof (value as { decimal?: unknown }).decimal === "string"
  );
}

function isBytesValue(value: unknown): value is BytesValue {
  return (
    value !== null &&
    typeof value === "object" &&
    (value as Record<PropertyKey, unknown>)[BYTES_VALUE_BRAND] === true &&
    typeof (value as { bytes?: unknown }).bytes === "string"
  );
}

function isRemovedDecimalCarrier(value: unknown): boolean {
  if (!isPlainObject(value) || isDecimalValue(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "decimal" && typeof value.decimal === "string";
}

function isRemovedBytesCarrier(value: unknown): boolean {
  if (!isPlainObject(value) || isBytesValue(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "bytes" && typeof value.bytes === "string";
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
 *  - a branded `decimal("...")` value → `{ decimal: "<v>" }`;
 *  - a branded `byteValue(...)` value → `{ bytes: "<base64>" }`;
 *  - a `Uint8Array` → `{ bytes: "<base64>" }` (the raw-bytes carrier);
 *  - JSON containers are scanned so function values fail closed at any depth;
 *  - everything else passes through verbatim.
 */
function toIrScalar(value: unknown): unknown {
  rejectNestedFunctionValues(value);
  if (isDecimalValue(value)) return { decimal: requireDecimalString(value.decimal) };
  if (isBytesValue(value)) return { bytes: requireBase64String(value.bytes) };
  if (typeof value === "bigint") {
    throw structuredError("OP_INVALID", 'bigint is not a value — use decimal("<n>")');
  }
  if (isRemovedDecimalCarrier(value)) {
    throw structuredError("OP_INVALID", 'the { decimal } carrier is removed — use decimal("<n>")');
  }
  if (isRemovedBytesCarrier(value)) {
    throw structuredError("OP_INVALID", "the { bytes } carrier is removed — use byteValue(...)");
  }
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

const JSON_DEFAULT_INTEGER_ERROR =
  "json default values support integers only (floats not yet supported)";
const JSON_DEFAULT_VALUE_ERROR =
  "json default values must be JSON values (null, boolean, integer, string, array, or object)";

function toIrJsonValue(value: unknown): unknown {
  rejectFunctionValue(value);
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (Number.isInteger(value) && Math.abs(value) < 2 ** 53) return value;
    throw structuredError("OP_INVALID", JSON_DEFAULT_INTEGER_ERROR);
  }
  if (Array.isArray(value)) return value.map((item) => toIrJsonValue(item));
  if (isPlainObject(value)) {
    if ("fn" in value && typeof value.fn === "string") {
      throw structuredError("OP_INVALID", "json default values cannot contain nested function defaults");
    }
    const out: Record<string, unknown> = {};
    for (const key of Object.keys(value).sort()) {
      out[key] = toIrJsonValue(value[key]);
    }
    return out;
  }
  throw structuredError("OP_INVALID", JSON_DEFAULT_VALUE_ERROR);
}

const DEFAULT_SCALAR_FNS = new Set([
  "coalesce",
  "nullif",
  "lower",
  "upper",
  "trim",
  "length",
  "abs",
  "mod",
  "round",
  "floor",
  "ceil",
  "substr",
  "replace",
]);

const DEFAULT_SYNTH_FNS = new Set([
  "now",
  "genRandomUuid",
  "concatWs",
  "splitPart",
]);

const IMMUTABLE_SCALAR_FNS = new Set([
  "coalesce",
  "nullif",
  "lower",
  "upper",
  "trim",
  "length",
  "abs",
  "mod",
  "round",
  "floor",
  "ceil",
  "substr",
  "replace",
]);

const IMMUTABLE_SYNTH_FNS = new Set([
  "concatWs",
  "splitPart",
]);

const IMMUTABLE_HELPERS =
  "lower/upper/trim/length/abs/coalesce/nullif/mod/round/floor/ceil/substr/replace/concatWs/splitPart";

function defaultBuilder(): DefaultBuilder {
  return Object.freeze({ fn, case: caseExpr });
}

function defaultFunctionValueError(): Error {
  return structuredError(
    "OP_INVALID",
    "function defaults must be authored with top-level value constructors, e.g. " +
      "`.default(now())` or `.default(genRandomUuid())`; " +
      "the old `{ fn: ... }` and bare native-symbol default forms are removed",
  );
}

function rejectRemovedDefaultFunctionValue(value: unknown): void {
  if (
    value === nativeDateNow ||
    value === nativeMathRandom ||
    (nativeCryptoRandomUUID !== undefined && value === nativeCryptoRandomUUID)
  ) {
    throw defaultFunctionValueError();
  }
}

function isExprNode(value: unknown): value is Node {
  return Boolean(value && typeof value === "object" && typeof (value as Node).node === "string");
}

function resolveDefaultExpr(slot: DefaultExprFn | ExprChainType | Node): Node {
  rejectRemovedDefaultFunctionValue(slot);
  const resolved =
    typeof slot === "function"
      ? slot(defaultBuilder())
      : slot;
  if (resolved instanceof ExprChainImpl) return resolved.__node;
  if (isExprNode(resolved)) return resolved;
  return exprArg(resolved);
}

function validateDefaultExpr(expr: Node): void {
  const walk = (node: unknown): void => {
    if (!node || typeof node !== "object" || typeof (node as Node).node !== "string") {
      throw structuredError("OP_INVALID", "default expression must be a closed Expr node");
    }
    const n = node as Node;
    switch (n.node) {
      case "colRef":
        throw structuredError("OP_INVALID", "a column default cannot reference a column");
      case "agg":
        throw structuredError("OP_INVALID", "a column default cannot use an aggregate");
      case "literal":
        return;
      case "fnCall": {
        if (typeof n.fn !== "string" || !DEFAULT_SCALAR_FNS.has(n.fn)) {
          throw structuredError(
            "OP_INVALID",
            "a column default cannot use volatile or vendor-only functions; use immutable c.fn helpers",
          );
        }
        if (!Array.isArray(n.args)) {
          throw structuredError("OP_INVALID", "default function expression args must be an array");
        }
        n.args.forEach(walk);
        return;
      }
      case "fnSynth": {
        if (typeof n.fn !== "string" || !DEFAULT_SYNTH_FNS.has(n.fn)) {
          throw structuredError("OP_INVALID", "a column default cannot use this synthesized function");
        }
        if (!Array.isArray(n.args)) {
          throw structuredError("OP_INVALID", "default synth expression args must be an array");
        }
        n.args.forEach(walk);
        return;
      }
      case "binOp":
        walk(n.lhs);
        walk(n.rhs);
        return;
      case "unaryOp":
        walk(n.operand);
        return;
      case "case": {
        if (!Array.isArray(n.branches)) {
          throw structuredError("OP_INVALID", "default CASE expression branches must be an array");
        }
        for (const branch of n.branches) {
          if (!isPlainObject(branch)) {
            throw structuredError("OP_INVALID", "default CASE branches must be { when, then } objects");
          }
          walk(branch.when);
          walk(branch.then);
        }
        if (n.else !== undefined && n.else !== null) walk(n.else);
        return;
      }
      case "cast":
        walk(n.operand);
        return;
      case "between":
        walk(n.operand);
        walk(n.low);
        walk(n.high);
        return;
      case "like":
        walk(n.operand);
        walk(n.pattern);
        return;
      case "distinctFrom":
        walk(n.left);
        walk(n.right);
        return;
      case "inList":
        walk(n.expr);
        return;
      case "pgRegexMatch":
      case "pgColumnSize":
      case "pgExtract":
      case "pgInterval":
      case "dialect":
        throw structuredError(
          "OP_INVALID",
          "a column default cannot use volatile, dialect-specific, or vendor-only expression nodes",
        );
      case "extract":
        throw structuredError("OP_INVALID", "a column default cannot use an EXTRACT expression");
      default:
        throw structuredError("OP_INVALID", `unsupported default expression node ${JSON.stringify(n.node)}`);
    }
  };
  walk(expr);
}

function defaultExprIr(slot: DefaultExprFn | ExprChainType | Node): Node {
  const expr = resolveDefaultExpr(slot);
  validateDefaultExpr(expr);
  return { expr };
}

function toIrDefault(value: DefaultValue | DefaultExprFn | ExprChainType | Node): Node {
  if (typeof value === "function" || value instanceof ExprChainImpl || isExprNode(value)) {
    return defaultExprIr(value as DefaultExprFn | ExprChainType | Node);
  }
  if (isNextvalDefault(value)) {
    return { nextval: compact({ name: value.name, schema: value.schema }) };
  }
  if (value && typeof value === "object" && "fn" in value && typeof value.fn === "string") {
    throw defaultFunctionValueError();
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return { container: "array" };
    rejectNestedFunctionValues(value);
    return { json: toIrJsonValue(value) };
  }
  if (isDecimalValue(value)) return { literal: { value: toIrScalar(value) } };
  if (isBytesValue(value)) return { literal: { value: toIrScalar(value) } };
  if (isPlainObject(value)) {
    if (Object.keys(value).length === 0) return { container: "object" };
    if (isRemovedDecimalCarrier(value)) return { literal: { value: toIrScalar(value) } };
    if (isRemovedBytesCarrier(value)) return { literal: { value: toIrScalar(value) } };
    rejectNestedFunctionValues(value);
    return { json: toIrJsonValue(value) };
  }
  return { literal: { value: toIrScalar(value) } };
}

type NextvalDefaultMarker = NextvalDefault & {
  readonly [NEXTVAL_DEFAULT_MARKER]: true;
};

function isNextvalDefault(value: unknown): value is NextvalDefaultMarker {
  return (
    value !== null &&
    typeof value === "object" &&
    (value as Record<PropertyKey, unknown>)[NEXTVAL_DEFAULT_MARKER] === true
  );
}

export function nextval(name: string, opts: NextvalOptions = {}): NextvalDefault {
  requireString(name, "nextval(name)");
  if (opts === null || typeof opts !== "object" || Array.isArray(opts)) {
    throw structuredError("OP_INVALID", "nextval(name, opts): opts must be { schema?: string }");
  }
  if (opts.schema !== undefined) requireString(opts.schema, "nextval(name, { schema })");
  return Object.freeze({
    [NEXTVAL_DEFAULT_MARKER]: true,
    name,
    schema: opts.schema,
  }) as unknown as NextvalDefault;
}

export function now(): ExprChainType {
  return chain({ node: "fnSynth", fn: "now", args: [] });
}

export function genRandomUuid(): ExprChainType {
  return chain({ node: "fnSynth", fn: "genRandomUuid", args: [] });
}

export function currentSetting(name: string, opts: CurrentSettingOptions = {}): ExprChainType {
  requireString(name, "currentSetting(name)");
  if (opts === null || typeof opts !== "object" || Array.isArray(opts)) {
    throw structuredError("OP_INVALID", "currentSetting(name, opts): opts must be { missingOk?: boolean }");
  }
  const missingOk = requireOptionalBoolean(opts.missingOk, "currentSetting(name, { missingOk })");
  return chain({
    node: "fnCall",
    fn: "currentSetting",
    args: missingOk === undefined
      ? [{ node: "literal", value: name }]
      : [{ node: "literal", value: name }, { node: "literal", value: missingOk }],
  });
}

export function currentUser(): ExprChainType {
  return chain({ node: "fnCall", fn: "currentUser", args: [] });
}

export function interval(duration: Duration): ExprChainType {
  return chain({
    node: "pgInterval",
    duration: pgDuration(duration),
  });
}

export const t: TypeLexicon = {
  id: (opts?: IdOptions) => {
    let col = new ColumnDefImpl("uuid").primaryKey().default(genRandomUuid());
    if (opts && opts.prefix !== undefined) {
      requireString(opts.prefix, "t.id({ prefix })");
      col = col.__withIdPrefix(opts.prefix);
    }
    return col;
  },
  text: (opts?: TextOptions) => textColumn(opts),
  textArray: () => new ColumnDefImpl("textArray"),
  numeric: (opts = {}) => {
    requirePlainObject(opts, "t.numeric(opts)");
    const precision = requireOptionalPositiveInteger(opts.precision, "t.numeric({ precision })") ?? 38;
    const scale = requireOptionalNonNegativeInteger(opts.scale, "t.numeric({ scale })") ?? 9;
    return new ColumnDefImpl({ decimal: { precision, scale } } as ColType);
  },
  char: (opts) => {
    requirePlainObject(opts, "t.char(opts)");
    const n = requireOptionalPositiveInteger(opts.length, "t.char({ length })");
    if (n === undefined) {
      throw structuredError("OP_INVALID", "t.char({ length }) requires length");
    }
    return new ColumnDefImpl({ char: { length: n } } as ColType);
  },
  timestamp: () => new ColumnDefImpl("timestamp"),
  date: () => new ColumnDefImpl("date" as ColType),
  uuid: () => new ColumnDefImpl("uuid"),
  bytes: () => new ColumnDefImpl("bytes"),
  boolean: () => new ColumnDefImpl("boolean"),
  json: () => new ColumnDefImpl("json"),
  ref: (targetTable) => {
    requireString(targetTable, "t.ref(target)");
    return new ColumnDefImpl({ ref: { references: targetTable } } as ColType);
  },
  vector: (opts: VectorOptions) => {
    requirePlainObject(opts, "t.vector(opts)");
    const n = requireOptionalPositiveInteger(opts.dimensions, "t.vector({ dimensions })");
    if (n === undefined) {
      throw structuredError("OP_INVALID", "t.vector({ dimensions }) requires dimensions");
    }
    let col = new ColumnDefImpl({ vector: { vector: n } } as ColType);
    if (opts.metric !== undefined) {
      requireString(opts.metric, "t.vector({ metric })");
      // LOW-1: a closed-set check on the metric token gives a friendly OP_INVALID at
      // authoring time instead of a cryptic serde "unknown variant" at the Rust
      // deserialize seam (the engine's closed `VectorMetric` enum stays authoritative).
      if (!VECTOR_METRICS.includes(opts.metric)) {
        throw structuredError(
          "OP_INVALID",
          `t.vector({ dimensions, metric }): metric must be one of ${VECTOR_METRICS.join(" | ")}, ` +
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
  int: () => new ColumnDefImpl("int"),
  bigInt: () => new ColumnDefImpl("bigInt"),
  real: () => new ColumnDefImpl("real"),
  double: () => new ColumnDefImpl("double"),
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

export function __pgSchema(name: string): SchemaHandle {
  requireString(name, "schema(name)");
  let handle: SchemaHandle;
  const dropped: DroppedSchemaHandle = {
    name,
    create(args: SchemaCreateArgs = {}) {
      recordCreateSchema(name, args);
      return handle;
    },
  };
  handle = {
    name,
    create(args: SchemaCreateArgs = {}) {
      recordCreateSchema(name, args);
      return handle;
    },
    drop(args: SchemaDropArgs = {}) {
      recordDropSchema(name, args);
      return dropped;
    },
  };
  return handle;
}

export function __pgExtension(name: string): ExtensionHandle {
  requireString(name, "extension(name)");
  let handle: ExtensionHandle;
  const dropped: DroppedExtensionHandle = {
    name,
    create(args: ExtensionCreateArgs = {}) {
      recordCreateExtension(name, args);
      return handle;
    },
  };
  handle = {
    name,
    create(args: ExtensionCreateArgs = {}) {
      recordCreateExtension(name, args);
      return handle;
    },
    drop(args: ExtensionDropArgs = {}) {
      recordDropExtension(name, args);
      return dropped;
    },
  };
  return handle;
}

export function __pgRole(name: string): RoleHandle {
  requireString(name, "role(name)");
  let handle: RoleHandle;
  const dropped: DroppedRoleHandle = {
    name,
    create(args: RoleCreateArgs = {}) {
      recordCreateRole(name, args);
      return handle;
    },
  };
  handle = {
    name,
    create(args: RoleCreateArgs = {}) {
      recordCreateRole(name, args);
      return handle;
    },
    setOptions(args: RoleSetOptionsArgs) {
      recordSetRoleOptions(name, args);
      return handle;
    },
    drop(args: RoleDropArgs = {}) {
      recordDropRole(name, args);
      return dropped;
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

type InListScalarKind = "string" | "number" | "boolean" | "null";

function inListScalarKind(value: unknown, label: string): InListScalarKind {
  if (value === null) return "null";
  switch (typeof value) {
    case "string":
      if (value.length === 0) {
        throw structuredError("OP_INVALID", `${label} must be non-empty`);
      }
      if (value.includes("\0")) {
        throw structuredError("OP_INVALID", `${label} must not contain a NUL byte`);
      }
      return "string";
    case "number":
      if (!Number.isFinite(value)) {
        throw structuredError("OP_INVALID", `${label} must be a finite number`);
      }
      return "number";
    case "boolean":
      return "boolean";
    default:
      throw structuredError(
        "OP_INVALID",
        `${label} must be a Scalar (string, number, boolean, or null); got ${typeof value}`,
      );
  }
}

function scalarLiteralArray(values: unknown, what: string): unknown[] {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a Scalar[]`);
  }
  let kind: InListScalarKind | undefined;
  return values.map((v, i) => {
    const elemKind = inListScalarKind(v, `${what}[${i}]`);
    if (kind === undefined) {
      kind = elemKind;
    } else if (elemKind !== kind) {
      throw structuredError("OP_INVALID", `${what} list must be homogeneous; ${what}[${i}] is ${elemKind}, expected ${kind}`);
    }
    return toIrScalar(v);
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

const portableExtractFields = ["year", "month", "day", "hour", "minute", "dow"] as const;
type PortableExtractField = typeof portableExtractFields[number];
const portableExtractFieldSet = new Set<string>(portableExtractFields);

const pgExtractFields = [
  "second",
  "doy",
  "epoch",
  "quarter",
  "week",
  "isodow",
  "isoyear",
  "century",
  "decade",
  "millennium",
  "microseconds",
  "milliseconds",
  "timezone",
  "timezone_hour",
  "timezone_minute",
] as const;
type PgExtractFieldToken = typeof pgExtractFields[number];
const pgExtractFieldSet = new Set<string>(pgExtractFields);

const castTargets = ["text", "int", "real", "boolean", "bytes", "uuid"] as const;
const castTargetSet = new Set<string>(castTargets);

function castTarget(args: unknown): CastTarget {
  if (!isPlainObject(args)) {
    throw structuredError("OP_INVALID", "cast(args): args must be { to }");
  }
  const to = args.to;
  if (typeof to !== "string" || !castTargetSet.has(to)) {
    throw structuredError(
      "OP_INVALID",
      `cast({ to }): to must be one of ${castTargets.map((t) => JSON.stringify(t)).join(", ")}; got ${JSON.stringify(to)}`,
    );
  }
  return to as CastTarget;
}

function extractField(field: unknown, what = "c.fn.extract(field, expr)"): PortableExtractField {
  if (typeof field !== "string" || !portableExtractFieldSet.has(field)) {
    throw structuredError(
      "OP_INVALID",
      `${what}: field must be one of ${portableExtractFields.map((f) => JSON.stringify(f)).join(", ")}; got ${JSON.stringify(field)}`,
    );
  }
  return field as PortableExtractField;
}

function pgExtractField(field: unknown): PortableExtractField | PgExtractFieldToken {
  if (typeof field === "string" && portableExtractFieldSet.has(field)) {
    return field as PortableExtractField;
  }
  if (typeof field === "string" && pgExtractFieldSet.has(field)) {
    return field as PgExtractFieldToken;
  }
  throw structuredError(
    "OP_INVALID",
    `c.pg.extract(field, expr): field must be one of ${[...portableExtractFields, ...pgExtractFields].map((f) => JSON.stringify(f)).join(", ")}; got ${JSON.stringify(field)}`,
  );
}

const durationFields = ["years", "months", "days", "hours", "minutes", "seconds"] as const;

function pgDuration(duration: unknown): Duration {
  if (!isPlainObject(duration)) {
    throw structuredError("OP_INVALID", `interval(duration): duration must be an object; got ${typeof duration}`);
  }

  const normalized: Duration = {};
  for (const key of durationFields) {
    const value = duration[key];
    if (value === undefined) continue;
    if (!Number.isInteger(value)) {
      throw structuredError(
        "OP_INVALID",
        `interval(duration): ${key} must be an integer; got ${JSON.stringify(value)}`,
      );
    }
    normalized[key] = value as number;
  }

  for (const key of Object.keys(duration)) {
    if (!(durationFields as readonly string[]).includes(key)) {
      throw structuredError(
        "OP_INVALID",
        `interval(duration): unknown duration field ${JSON.stringify(key)}`,
      );
    }
  }

  if (Object.keys(normalized).length === 0) {
    throw structuredError("OP_INVALID", "interval(duration): at least one duration field is required");
  }

  return normalized;
}

class ExprChainImpl implements ExprChainType {
  __node: Node;
  constructor(node: Node) {
    this.__node = node;
  }
  private bin(op: string, x: unknown): ExprChainImpl {
    return chain({ node: "binOp", op, lhs: this.__node, rhs: exprArg(x) });
  }
  eq(x: unknown) {
    if (x === null) {
      throw structuredError("OP_INVALID", "eq(null) is always UNKNOWN in SQL — use isNull()");
    }
    return this.bin("eq", x);
  }
  ne(x: unknown) {
    if (x === null) {
      throw structuredError("OP_INVALID", "ne(null) is always UNKNOWN in SQL — use isNotNull()");
    }
    return this.bin("ne", x);
  }
  lt(x: unknown) { return this.bin("lt", x); }
  le(x: unknown) { return this.bin("le", x); }
  gt(x: unknown) { return this.bin("gt", x); }
  ge(x: unknown) { return this.bin("ge", x); }
  and(...es: unknown[]) {
    let acc = this.__node;
    for (const e of es) acc = { node: "binOp", op: "and", lhs: acc, rhs: exprArg(e) };
    return chain(acc);
  }
  or(...es: unknown[]) {
    let acc = this.__node;
    for (const e of es) acc = { node: "binOp", op: "or", lhs: acc, rhs: exprArg(e) };
    return chain(acc);
  }
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
  cast(args: { to: CastTarget }) {
    return chain({ node: "cast", operand: this.__node, target: castTarget(args) });
  }
  // Portable predicate nodes (§3.4). `between`/`like` render identical syntax on
  // all three dialects; `distinctFrom` is portably named but per-dialect rendered
  // (PG/SQLite `IS DISTINCT FROM` vs MySQL `NOT (x <=> y)`) — the engine owns it.
  between(low: unknown, high: unknown) {
    return chain({ node: "between", operand: this.__node, low: exprArg(low), high: exprArg(high) });
  }
  like(pattern: unknown) {
    return chain({ node: "like", operand: this.__node, pattern: exprArg(pattern) });
  }
  "in"(values: readonly Scalar[]) {
    return chain({
      node: "inList",
      expr: this.__node,
      elems: scalarLiteralArray(values, ".in(values)"),
      negated: false,
    });
  }
  notIn(values: readonly Scalar[]) {
    return chain({
      node: "inList",
      expr: this.__node,
      elems: scalarLiteralArray(values, ".notIn(values)"),
      negated: true,
    });
  }
  distinctFrom(x: unknown) {
    return chain({ node: "distinctFrom", left: this.__node, right: exprArg(x) });
  }
  // PG-first chain operators (P0). Same IR nodes as the old `c.pg.*` helpers;
  // the dialect gate lives in the Rust validator (fail-closed off-target).
  regex(pattern: string) {
    return chain({ node: "pgRegexMatch", expr: this.__node, pattern: pgRegexPattern(pattern) });
  }
  columnSize() {
    return chain({ node: "pgColumnSize", expr: this.__node });
  }
}

export function check(name: string, expr: CheckExprFn): CheckDef {
  requireString(name, "check(name, expr)");
  if (typeof expr !== "function") {
    throw structuredError("OP_INVALID", "check(name, expr): expr must be a (c) => Expr callback");
  }
  return { name, expr };
}

export function lit(value: ScalarValue): ExprChainType {
  return chain({ node: "literal", value: toIrScalar(value) });
}

/**
 * The one Layer-2 portability escape (design §3.4): a per-dialect VALUE
 * divergence in an expression position. Each leg is an expression (a `(c) =>
 * Expr` chain node, another combinator, or a bare scalar), and the engine
 * renders the leg matching the target dialect — the dialect's own leg if
 * present, else `default`:
 *
 * ```ts
 * default(dialect({ pg: genRandomUuid(), sqlite: now(), mysql: myUuid }))
 * dialect({ default: lit(0), pg: c("n") })   // pg leg on PG, default(0) elsewhere
 * ```
 *
 * At least one leg (`default`/`pg`/`sqlite`/`mysql`) must be present; the legs
 * record in full in the checksummed IR as the `dialect` node in canonical order.
 * The engine's validate applies the per-TARGET scope math: a target with no own
 * leg and no `default` is refused (`EXPR_NOT_PORTABLE`). RATCHET (P11): each leg
 * is a ratcheted budget counter — the budget mechanism is a later phase.
 */
export function dialect(legs: {
  default?: unknown;
  pg?: unknown;
  sqlite?: unknown;
  mysql?: unknown;
}): ExprChainType {
  if (legs === null || typeof legs !== "object") {
    throw structuredError(
      "OP_INVALID",
      "dialect(legs): legs must be an object with default/pg/sqlite/mysql expression legs",
    );
  }
  const node: Node = { node: "dialect" };
  // Canonical leg order: default, pg, sqlite, mysql (mirrors the IR field order).
  let count = 0;
  for (const leg of ["default", "pg", "sqlite", "mysql"] as const) {
    const value = (legs as Record<string, unknown>)[leg];
    if (value !== undefined) {
      node[leg] = exprArg(value);
      count++;
    }
  }
  if (count === 0) {
    throw structuredError(
      "OP_INVALID",
      "dialect(legs): at least one leg (default/pg/sqlite/mysql) must be present",
    );
  }
  return chain(node);
}

const fn: FnNamespace = {
  lower: (e) => chain({ node: "fnCall", fn: "lower", args: [exprArg(e)] }),
  upper: (e) => chain({ node: "fnCall", fn: "upper", args: [exprArg(e)] }),
  trim: (e) => chain({ node: "fnCall", fn: "trim", args: [exprArg(e)] }),
  length: (e) => chain({ node: "fnCall", fn: "length", args: [exprArg(e)] }),
  abs: (e) => chain({ node: "fnCall", fn: "abs", args: [exprArg(e)] }),
  coalesce: (...args) => chain({ node: "fnCall", fn: "coalesce", args: args.map(exprArg) }),
  nullif: (a, b) => chain({ node: "fnCall", fn: "nullif", args: [exprArg(a), exprArg(b)] }),
  mod: (a, b) => chain({ node: "fnCall", fn: "mod", args: [exprArg(a), exprArg(b)] }),
  round: (x, n) =>
    chain({
      node: "fnCall",
      fn: "round",
      args: n === undefined ? [exprArg(x)] : [exprArg(x), exprArg(n)],
    }),
  floor: (x) => chain({ node: "fnCall", fn: "floor", args: [exprArg(x)] }),
  ceil: (x) => chain({ node: "fnCall", fn: "ceil", args: [exprArg(x)] }),
  substr: (s, start, len) =>
    chain({
      node: "fnCall",
      fn: "substr",
      args: len === undefined ? [exprArg(s), exprArg(start)] : [exprArg(s), exprArg(start), exprArg(len)],
    }),
  replace: (s, from, to) => chain({ node: "fnCall", fn: "replace", args: [exprArg(s), exprArg(from), exprArg(to)] }),
  extract: (field, expr) => chain({
    node: "extract",
    field: extractField(field),
    from: exprArg(expr),
  }),
  concatWs: (sep, ...parts) => chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },
};

const immutableFn: ImmutableFnNamespace = Object.freeze({
  lower: fn.lower,
  upper: fn.upper,
  trim: fn.trim,
  length: fn.length,
  abs: fn.abs,
  coalesce: fn.coalesce,
  nullif: fn.nullif,
  mod: fn.mod,
  round: fn.round,
  floor: fn.floor,
  ceil: fn.ceil,
  substr: fn.substr,
  replace: fn.replace,
  extract: fn.extract,
  concatWs: fn.concatWs,
  splitPart: fn.splitPart,
});

// The `c.agg.*` PORTABLE aggregate namespace (§3.4/§3.6). `count()` (no arg)
// records `count(*)`; a present arg records `<func>(<arg>)`. The optional
// `{ distinct: true }` sets the `distinct` flag (skipped on the wire when false).
// count/sum/avg/min/max are byte-identical SQL on PG/SQLite/MySQL — no dialect
// gate. The "aggregate only valid in a grouped/SELECT context" check is a
// Phase-2 obligation; the recorder builds the node structurally here.
function aggNode(func: "count" | "sum" | "avg" | "min" | "max", expr: unknown, opts?: { distinct?: boolean }): ExprChainType {
  const node: Node = { node: "agg", func };
  if (expr !== undefined) node.arg = exprArg(expr);
  if (opts && opts.distinct === true) node.distinct = true;
  return chain(node);
}

const agg: AggNamespace = {
  count: (expr, opts) => aggNode("count", expr, opts),
  sum: (expr, opts) => aggNode("sum", expr, opts),
  avg: (expr, opts) => aggNode("avg", expr, opts),
  min: (expr, opts) => aggNode("min", expr, opts),
  max: (expr, opts) => aggNode("max", expr, opts),
};

const pgExpr: PgExprNamespace = {
  extract: (field, expr) => {
    const f = pgExtractField(field);
    return chain({
      node: portableExtractFieldSet.has(f) ? "extract" : "pgExtract",
      field: f,
      from: exprArg(expr),
    });
  },
};

type CaseExprArgs = {
  branches: Array<{ when: unknown; then: unknown }>;
  else?: unknown;
};

function caseExpr(args: CaseExprArgs): ExprChainType {
  const shape = "c.case({ branches: [{ when, then }], else? })";
  if (!isPlainObject(args)) {
    throw structuredError("OP_INVALID", `${shape}: args must be an object`);
  }
  const branches = args.branches;
  if (!Array.isArray(branches) || branches.length === 0) {
    throw structuredError(
      "OP_INVALID",
      `${shape}: branches must be a non-empty array of { when, then } objects`,
    );
  }
  const node: Node = {
    node: "case",
    branches: branches.map((branch, i) => {
      if (
        !isPlainObject(branch) ||
        !Object.prototype.hasOwnProperty.call(branch, "when") ||
        !Object.prototype.hasOwnProperty.call(branch, "then")
      ) {
        throw structuredError(
          "OP_INVALID",
          `${shape}: branches[${i}] must be an object with when and then`,
        );
      }
      return { when: exprArg(branch.when), then: exprArg(branch.then) };
    }),
  };
  if (args.else !== undefined) node.else = exprArg(args.else);
  return chain(node);
}

function makeColumnAccessor(): (first: string, second?: string) => ExprChainType {
  // One-arg `c("col")` → unqualified colRef (byte-identical to the pre-
  // qualification wire shape). Two-arg `c("table", "col")` → qualified colRef
  // (§3.4, the join-ON fix): the wire `colRef` node gains an optional `table`.
  return ((first: string, second?: string) => {
    if (second === undefined) {
      requireString(first, 'c("name")');
      return chain({ node: "colRef", name: first });
    }
    requireString(first, 'c("table", "col")');
    requireString(second, 'c("table", "col")');
    return chain({ node: "colRef", table: first, name: second });
  });
}

function immutableExprBuilder(): IndexExprBuilder {
  const c = makeColumnAccessor() as unknown as IndexExprBuilder;
  c.case = caseExpr;
  c.fn = immutableFn;
  return Object.freeze(c);
}

function checkWithPgBuilder(): CheckBuilderWithPg {
  const c = makeColumnAccessor() as unknown as CheckBuilderWithPg;
  c.case = caseExpr;
  c.fn = immutableFn;
  c.pg = pgExpr;
  return Object.freeze(c);
}

function domainValueBuilder(): DomainValueBuilder {
  const v = chain({ node: "colRef", name: "VALUE" }) as unknown as DomainValueBuilder;
  v.case = caseExpr;
  v.fn = immutableFn;
  v.pg = pgExpr;
  return Object.freeze(v);
}

function makeBuilder(): ExprBuilder {
  const c = makeColumnAccessor() as unknown as ExprBuilder;
  c.case = caseExpr;
  c.fn = fn;
  c.agg = agg;
  c.pg = pgExpr;
  return c;
}

// The standalone `c.case` / `c.fn` / `c.pg` builders surfaced at a value position
// (`cCase(...)`, `cFn.splitPart(...)`, `cPg.extract(...)`) — the SAME objects installed on the
// `(c) => Expr` builder above. These are exported for the engine-embedded
// recorder bundle (`src/embedded-recorder.ts`, the `include_str!`'d artifact),
// which requires the full engine-consumed surface. Not re-exported through the
// SDK public `.` entry (`index.ts`); a value-position namespace is a Phase-2
// surface decision.
export const cCase = caseExpr;
export const cFn = fn;
export const cAgg = agg;
export const cPg = pgExpr;

function resolveExpr(slot: ExprFn | ExprChainType | Node | undefined): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  if (typeof slot === "function") return exprArg(slot(makeBuilder()));
  if (slot instanceof ExprChainImpl) return slot.__node;
  if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") return slot as Node;
  throw structuredError("OP_INVALID", "expression slot must be a (c) => Expr callback or a built expression");
}

type ImmutableExprSlot = IndexExprFn | GeneratedColumnExprFn | CheckExprFn | ExprChainType | Node | undefined;

function resolveImmutableExpr(slot: ImmutableExprSlot, position: string): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  let resolved: Node;
  if (typeof slot === "function") {
    resolved = exprArg((slot as (c: IndexExprBuilder) => unknown)(immutableExprBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") {
    resolved = slot as Node;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (c) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position);
  return resolved;
}

function resolveCheckWithPg(
  slot: PgCheckExprFn | ExprChainType | Node | undefined,
  position: string,
): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  let resolved: Node;
  if (typeof slot === "function") {
    resolved = exprArg((slot as (c: CheckBuilderWithPg) => unknown)(checkWithPgBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") {
    resolved = slot as Node;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (c) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position, { allowPgImmutable: true });
  return resolved;
}

function validateDomainCheckColRefs(expr: Node, position: string): void {
  const walk = (value: unknown): void => {
    if (Array.isArray(value)) {
      value.forEach(walk);
      return;
    }
    if (!isPlainObject(value)) return;
    if (value.node === "colRef") {
      const table = (value as { table?: unknown }).table;
      if (value.name !== "VALUE" || (table !== undefined && table !== null)) {
        throw structuredError(
          "OP_INVALID",
          `${position} may reference only the domain VALUE pseudo-column; non-VALUE colRef nodes are not valid in domain SQL`,
        );
      }
    }
    Object.values(value).forEach(walk);
  };
  walk(expr);
}

function resolveDomainCheck(
  slot: DomainCheckFn | ExprChainType | Node | undefined,
  position: string,
): Node | undefined {
  if (slot === undefined || slot === null) return undefined;
  let resolved: Node;
  if (typeof slot === "function") {
    resolved = exprArg((slot as (v: DomainValueBuilder) => unknown)(domainValueBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof (slot as Node).node === "string") {
    resolved = slot as Node;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (v) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position, { allowPgImmutable: true });
  validateDomainCheckColRefs(resolved, position);
  return resolved;
}

type CheckExprSlot = CheckExprFn | PgCheckExprFn | ExprChainType | Node | undefined;
type CheckExprResolver = (slot: CheckExprSlot, position: string) => Node | undefined;

const resolveCoreCheckExpr: CheckExprResolver = (slot, position) =>
  resolveImmutableExpr(slot as CheckExprFn | ExprChainType | Node | undefined, position);

const resolvePgCheckExpr: CheckExprResolver = (slot, position) =>
  resolveCheckWithPg(slot as PgCheckExprFn | ExprChainType | Node | undefined, position);

function rejectImmutableExpr(position: string, reason: string): never {
  throw structuredError(
    "OP_INVALID",
    `${position} must use only immutable expressions: column refs, literals, CASE, operators, and immutable c.fn helpers ` +
      `(${IMMUTABLE_HELPERS}); ${reason}`,
  );
}

function validateImmutableExpr(expr: Node, position: string, opts: { allowPgImmutable?: boolean } = {}): void {
  const rejectPgNode = (nodeName: string): void => {
    rejectImmutableExpr(position, `${nodeName} is PG-vendor and non-portable`);
  };
  const walk = (node: unknown): void => {
    if (!node || typeof node !== "object" || typeof (node as Node).node !== "string") {
      rejectImmutableExpr(position, "found a non-expression value");
    }
    const n = node as Node;
    switch (n.node) {
      case "colRef":
      case "literal":
        return;
      case "agg":
        rejectImmutableExpr(position, "aggregates are not allowed here");
      case "fnCall": {
        if (n.fn === "currentSetting" || n.fn === "currentUser") {
          rejectImmutableExpr(position, `${String(n.fn)} is PG-vendor and non-portable`);
        }
        if (typeof n.fn !== "string" || !IMMUTABLE_SCALAR_FNS.has(n.fn)) {
          rejectImmutableExpr(position, `function ${JSON.stringify(n.fn)} is not an immutable c.fn helper`);
        }
        if (!Array.isArray(n.args)) {
          rejectImmutableExpr(position, "function expression args must be an array");
        }
        n.args.forEach(walk);
        return;
      }
      case "fnSynth": {
        if (n.fn === "now" || n.fn === "genRandomUuid") {
          rejectImmutableExpr(position, `${String(n.fn)} is volatile`);
        }
        if (typeof n.fn !== "string" || !IMMUTABLE_SYNTH_FNS.has(n.fn)) {
          rejectImmutableExpr(position, `synthesized function ${JSON.stringify(n.fn)} is not immutable here`);
        }
        if (!Array.isArray(n.args)) {
          rejectImmutableExpr(position, "synthesized function expression args must be an array");
        }
        n.args.forEach(walk);
        return;
      }
      case "binOp":
        walk(n.lhs);
        walk(n.rhs);
        return;
      case "unaryOp":
        walk(n.operand);
        return;
      case "case": {
        if (!Array.isArray(n.branches)) {
          rejectImmutableExpr(position, "CASE expression branches must be an array");
        }
        for (const branch of n.branches) {
          if (!isPlainObject(branch)) {
            rejectImmutableExpr(position, "CASE branches must be { when, then } objects");
          }
          walk(branch.when);
          walk(branch.then);
        }
        if (n.else !== undefined && n.else !== null) walk(n.else);
        return;
      }
      case "cast":
        walk(n.operand);
        return;
      case "between":
        walk(n.operand);
        walk(n.low);
        walk(n.high);
        return;
      case "like":
        walk(n.operand);
        walk(n.pattern);
        return;
      case "distinctFrom":
        walk(n.left);
        walk(n.right);
        return;
      case "inList":
        walk(n.expr);
        return;
      case "pgRegexMatch":
        if (!opts.allowPgImmutable) rejectPgNode("pgRegexMatch");
        walk(n.expr);
        if (typeof n.pattern !== "string") {
          rejectImmutableExpr(position, "pgRegexMatch pattern must be a string");
        }
        return;
      case "pgColumnSize":
        if (!opts.allowPgImmutable) rejectPgNode("pgColumnSize");
        walk(n.expr);
        return;
      case "extract":
        if (typeof n.field !== "string" || !portableExtractFieldSet.has(n.field)) {
          rejectImmutableExpr(position, `extract field ${JSON.stringify(n.field)} is not portable here`);
        }
        walk(n.from);
        return;
      case "pgExtract":
        if (!opts.allowPgImmutable) rejectPgNode("pgExtract");
        if (typeof n.field !== "string" || !pgExtractFieldSet.has(n.field)) {
          rejectImmutableExpr(position, `pgExtract field ${JSON.stringify(n.field)} is not a PG extract field`);
        }
        walk(n.from);
        return;
      case "pgInterval":
        if (!opts.allowPgImmutable) rejectPgNode("pgInterval");
        if (!isPlainObject(n.duration)) {
          rejectImmutableExpr(position, "pgInterval duration must be an object");
        }
        try {
          pgDuration(n.duration);
        } catch (error) {
          rejectImmutableExpr(position, error instanceof Error ? error.message : "pgInterval duration is invalid");
        }
        return;
      case "dialect":
        for (const leg of ["default", "pg", "sqlite", "mysql"] as const) {
          if (n[leg] !== undefined && n[leg] !== null) walk(n[leg]);
        }
        return;
      default:
        rejectImmutableExpr(position, `unsupported expression node ${JSON.stringify(n.node)}`);
    }
  };
  walk(expr);
}

/** Internal hook used only by the `@zeroship/migrate/pg` subpath. */
export function __pgResolveExpr(slot: ExprFn | ExprChainType | Expr | undefined): Node | undefined {
  return resolveExpr(slot as ExprFn | ExprChainType | Node | undefined);
}

function resolveSetValue(value: DmlSetValue): unknown {
  const synth = nativeFnSynthNode(value);
  if (synth !== undefined) return synth;
  if (typeof value === "function") return resolveExpr(value as ExprFn)!;
  return toIrValue(value);
}

function resolveSet(set: Record<string, DmlSetValue>): Record<string, unknown> {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column → DML value");
  }
  const out: Record<string, unknown> = {};
  for (const col of Object.keys(set)) out[col] = resolveSetValue(set[col]);
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

function partitionSpecToIr(spec: PartitionByInput | undefined, what: string): Node | undefined {
  if (spec === undefined) return undefined;
  if (!spec || typeof spec !== "object") {
    throw structuredError("OP_INVALID", `${what} must be exactly one of { range }, { list }, or { hash }`);
  }
  const shape = spec as { range?: unknown; list?: unknown; hash?: unknown; whenUnsupported?: unknown };
  const variants = (shape.range !== undefined ? 1 : 0) + (shape.list !== undefined ? 1 : 0) + (shape.hash !== undefined ? 1 : 0);
  if (variants !== 1) {
    throw structuredError("OP_INVALID", `${what} must be exactly one of { range }, { list }, or { hash }`);
  }
  const collapse = shape.whenUnsupported === undefined ? undefined : shape.whenUnsupported;
  if (collapse !== undefined && collapse !== "collapse") {
    throw structuredError("OP_INVALID", `${what}.whenUnsupported must be "collapse" when present`);
  }
  const affirmation = { collapse: collapse === "collapse" };
  if (shape.range !== undefined) return { kind: "range", columns: stringArray(shape.range, `${what}.range`), ...affirmation };
  if (shape.list !== undefined) return { kind: "list", columns: stringArray(shape.list, `${what}.list`), ...affirmation };
  return { kind: "hash", columns: stringArray(shape.hash, `${what}.hash`), ...affirmation };
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

function partitionBoundToIr(args: PartitionBoundArgs | unknown): Node {
  if (!args || typeof args !== "object") {
    throw structuredError(
      "OP_INVALID",
      "table(parent).partition(name).create(bound) needs a bounds object",
    );
  }
  const bounds = args as { from?: unknown; to?: unknown; in?: unknown; modulus?: unknown; remainder?: unknown; default?: unknown };
  const hasRange = bounds.from !== undefined || bounds.to !== undefined;
  const hasList = bounds.in !== undefined;
  const hasHash = bounds.modulus !== undefined || bounds.remainder !== undefined;
  const hasDefault = bounds.default !== undefined;
  const variantCount = (hasRange ? 1 : 0) + (hasList ? 1 : 0) + (hasHash ? 1 : 0) + (hasDefault ? 1 : 0);
  if (variantCount !== 1) {
    throw structuredError(
      "OP_INVALID",
      "partition bounds must be exactly one of { from, to }, { in }, { modulus, remainder }, or { default: true }",
    );
  }
  if (hasDefault) {
    if (bounds.default !== true) {
      throw structuredError("OP_INVALID", "partition bounds.default must be true");
    }
    return { kind: "default" };
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
  emitCreateEnum({
    name,
    schema: args.schema,
    values: enumValues,
  });
}

function recordDropEnum(name: string, args: DropEnumArgs = {}): void {
  requireString(name, "enumType(name).drop()");
  emitDropEnum({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function recordCreateDomain(name: string, args: CreateDomainArgs): void {
  requireString(name, "domain(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "domain(name).create({ as, ... }) needs an object");
  }
  if (args.notNull !== undefined && typeof args.notNull !== "boolean") {
    throw structuredError("OP_INVALID", "domain(name).create({ notNull }): notNull must be a boolean");
  }
  emitCreateDomain({
    name,
    schema: args.schema,
    as: colTypeOf(args.as),
    check: resolveDomainCheck(args.check as DomainCheckFn | ExprChainType | Node | undefined, "domain(name).create({ check })"),
    default: args.default === undefined ? undefined : toIrDefault(args.default),
    notNull: args.notNull,
  });
}

function recordDropDomain(name: string, args: DropDomainArgs = {}): void {
  requireString(name, "domain(name).drop()");
  emitDropDomain({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
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
  emitCreateSequence({
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
  });
}

function recordAlterSequence(name: string, args: AlterSequenceArgs): void {
  requireString(name, "sequence(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).alter(args) needs an object");
  }
  const minValue = requireNullableSafeI64(args.minValue, "sequence.alter({ minValue })");
  const maxValue = requireNullableSafeI64(args.maxValue, "sequence.alter({ maxValue })");
  requireSequenceBounds(minValue, maxValue, "sequence.alter(args)");
  emitAlterSequence({
    name,
    schema: args.schema,
    increment: requireSequenceIncrement(args.increment, "sequence.alter({ increment })"),
    restart: requireNullableSafeI64(args.restart, "sequence.alter({ restart })"),
    minValue,
    maxValue,
    cache: requireSequenceCache(args.cache, "sequence.alter({ cache })"),
    cycle: args.cycle,
    ownedBy: args.ownedBy,
  });
}

function recordDropSequence(name: string, args: DropSequenceArgs = {}): void {
  requireString(name, "sequence(name)");
  emitDropSequence({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function requirePlainArgs(args: unknown, what: string): asserts args is Record<string, unknown> {
  if (args === null || typeof args !== "object") {
    throw structuredError("OP_INVALID", `${what} needs an object`);
  }
}

function recordCreateSchema(name: string, args: SchemaCreateArgs = {}): void {
  requireString(name, "schema(name)");
  requirePlainArgs(args, "schema(name).create(args)");
  emitCreateSchema({
    name,
    ifNotExists: args.ifNotExists,
    authorization: args.authorization,
  });
}

function recordDropSchema(name: string, args: SchemaDropArgs = {}): void {
  requireString(name, "schema(name)");
  requirePlainArgs(args, "schema(name).drop(args)");
  emitDropSchema({
    name,
    ifExists: args.ifExists,
    cascade: args.cascade,
  });
}

function recordCreateExtension(name: string, args: ExtensionCreateArgs = {}): void {
  requireString(name, "extension(name)");
  requirePlainArgs(args, "extension(name).create(args)");
  emitCreateExtension({
    name,
    ifNotExists: args.ifNotExists,
    schema: args.schema,
  });
}

function recordDropExtension(name: string, args: ExtensionDropArgs = {}): void {
  requireString(name, "extension(name)");
  requirePlainArgs(args, "extension(name).drop(args)");
  emitDropExtension({
    name,
    ifExists: args.ifExists,
  });
}

function recordCreateRole(name: string, args: RoleCreateArgs = {}): void {
  requireString(name, "role(name)");
  requirePlainArgs(args, "role(name).create(args)");
  emitCreateRole({
    name,
    login: args.login,
    password: args.password,
    bypassRls: args.bypassRls,
    createRole: args.createRole,
    createDb: args.createDb,
    superuser: args.superuser,
    inRole: args.inRole,
    setSearchPath: args.setSearchPath,
    ifNotExists: args.ifNotExists,
  });
}

function recordSetRoleOptions(name: string, args: RoleSetOptionsArgs): void {
  requireString(name, "role(name)");
  requirePlainArgs(args, "role(name).setOptions(args)");
  emitAlterRole({
    name,
    setSearchPath: args.setSearchPath,
    resetSearchPath: args.resetSearchPath,
  });
}

function recordDropRole(name: string, args: RoleDropArgs = {}): void {
  requireString(name, "role(name)");
  requirePlainArgs(args, "role(name).drop(args)");
  emitDropRole({
    name,
    ifExists: args.ifExists,
  });
}

function recordComment(target: CommentTargetArg, text: string | null): void {
  if (text !== null && typeof text !== "string") {
    throw structuredError("OP_INVALID", "comment text must be a string or null");
  }
  emitComment({
    target: commentTargetToIr(target),
    comment: text === null ? undefined : text,
  });
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

function recordCreateTable(
  name: string,
  args: CreateTableArgs,
  checkExprResolver: CheckExprResolver = resolveCoreCheckExpr,
): void {
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
    constraints.push(compact({
      name: ck.name,
      kind: { kind: "check", expr: checkExprResolver(ck.expr as CheckExprSlot, "check constraint") },
    }));
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
        deferrable: fkSpec.deferrable,
        initiallyDeferred: fkSpec.initiallyDeferred,
        schema: args.schema,
      }),
    );
  }
  for (const idx of args.indexes ?? []) {
    if (!Array.isArray(idx.on)) {
      throw structuredError("OP_INVALID", "create({ indexes }) index needs { on: IndexElementArg[] }");
    }
    indexes.push(
      compact({
        name: idx.name,
        columns: idx.on.map(indexElementToIr),
        unique: idx.unique,
        using: idx.using,
        where: resolveImmutableExpr(idx.where as IndexExprFn | ExprChainType | Node | undefined, "partial index predicate"),
        include: indexIncludeToIr(idx.include),
        with: indexWithToIr(idx.with),
        only: requireOptionalBoolean(idx.only, "index only"),
        nullsNotDistinct: requireOptionalBoolean(idx.nullsNotDistinct, "index nullsNotDistinct"),
      }),
    );
  }

  emitCreateTable({
    name,
    columns: cols,
    primaryKey,
    constraints: constraints.length ? constraints : undefined,
    indexes: indexes.length ? indexes : undefined,
    partitionBy: partitionSpecToIr(args.partitionBy, "create({ partitionBy })"),
    runtimeOptions: runtimeOptionsFromCreateArgs(args),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordCreatePartition(
  name: string,
  parent: string,
  bounds: Node,
  args: { ifNotExists?: boolean; schema?: string },
): void {
  emitCreatePartition({
    name,
    of: parent,
    bounds,
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordAttachPartition(
  parent: string,
  name: string,
  bound: Node,
  args: { schema?: string },
): void {
  emitAttachPartition({
    parent,
    name,
    bound,
    schema: args.schema,
  });
}

function recordDetachPartition(
  parent: string,
  name: string,
  args: { concurrently?: boolean; schema?: string },
): void {
  emitDetachPartition({
    parent,
    name,
    schema: args.schema,
    concurrently: args.concurrently,
  });
}

function recordDropPartition(
  parent: string,
  name: string,
  args: { ifExists?: boolean; cascade?: boolean; schema?: string },
): void {
  emitDropPartition({
    parent,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
    cascade: args.cascade,
  });
}

function recordSetTableOptions(
  table: string,
  args: { softDelete?: boolean; versioning?: boolean; strictness?: TableStrictness; schema?: string },
): void {
  emitSetTableOptions({
    table,
    options: runtimeOptionsPatchFromArgs(args),
    schema: args.schema,
  });
}

function recordSetRls(
  table: string,
  args: { enabled?: boolean; forced?: boolean; schema?: string },
): void {
  const enabled = requireOptionalBoolean(args.enabled, ".setRls({ enabled })");
  const forced = requireOptionalBoolean(args.forced, ".setRls({ forced })");
  if (enabled === undefined && forced === undefined) {
    throw structuredError("OP_INVALID", ".setRls needs at least one of { enabled, forced }");
  }
  emitSetRls({
    table,
    schema: args.schema,
    enabled,
    forced,
  });
}

function recordDropTable(
  table: string,
  args: { ifExists?: boolean; cascade?: boolean; schema?: string },
): void {
  emitDropTable({
    table,
    cascade: args.cascade,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function recordRenameTable(
  table: string,
  to: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  emitRenameTable({
    table,
    to,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function recordAddColumn(
  table: string,
  column: string,
  type: ColumnDefImpl,
  args: { ifNotExists?: boolean; schema?: string },
): void {
  emitAddColumn({
    table,
    column,
    ...type.__toAddColumnTail(),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
  // C2 — `.column(x).add({ type: t.text().unique() })` honors `.unique()`: emit a
  // follow-on unique constraint (mirroring the createTable per-column `.unique()`
  // image, which rides the column's `unique:true` field — but an ADD COLUMN has no
  // inline UNIQUE, so it lowers to a separate ADD CONSTRAINT).
  //
  // There is NO add-column PRIMARY KEY follow-on: primary key is create-time only
  // (`create({ primaryKey })` / the `.primaryKey()` facet on a create() column), so
  // the always-refused user PK constraint shape is deleted — `.primaryKey()` on an
  // added column records no pk op, and the `.unique()` follow-on is unconditional.
  if (type._unique) {
    emitAddColumnUnique({
      table,
      constraint: { kind: { kind: "unique", columns: [column] } },
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    });
  }
}

function recordDropColumn(
  table: string,
  column: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  emitDropColumn({
    table,
    column,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function recordRenameColumn(
  table: string,
  from: string,
  to: string,
  type: ColumnDefType,
  args: { schema?: string },
): void {
  emitRenameColumn({
    table,
    from,
    to,
    type: colTypeOf(type),
    schema: args.schema,
  });
}

function recordSetColumnType(
  table: string,
  name: string,
  change: { to: ColumnDefType; using?: ExprFn; schema?: string },
): void {
  requireColumnDef(change.to, ".column(name).setType({ to })");
  emitSetColumnType({
    table,
    column: name,
    toType: colTypeOf(change.to),
    using: resolveExpr(change.using),
    schema: change.schema,
  });
}

function recordSetColumnNotNull(table: string, name: string, args: { schema?: string }): void {
  emitSetColumnNotNull({ table, column: name, schema: args.schema });
}

function recordDropColumnNotNull(table: string, name: string, args: { schema?: string }): void {
  emitDropColumnNotNull({ table, column: name, schema: args.schema });
}

function recordSetColumnDefault(
  table: string,
  name: string,
  value: DefaultValue | DefaultExprFn | ExprChainType | Expr,
  args: { schema?: string },
): void {
  emitSetColumnDefault({
    table,
    column: name,
    value: toIrDefault(value),
    schema: args.schema,
  });
}

function recordDropColumnDefault(table: string, name: string, args: { schema?: string }): void {
  emitDropColumnDefault({ table, column: name, schema: args.schema });
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
  deferrable?: boolean;
  initiallyDeferred?: boolean;
  notValid?: boolean;
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
      deferrable: spec.deferrable,
      initiallyDeferred: spec.initiallyDeferred,
      // PG-only online constraint adoption; refused off Postgres at validate.
      notValid: spec.notValid,
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
    deferrable?: boolean;
    initiallyDeferred?: boolean;
    notValid?: boolean;
    ifNotExists?: boolean;
    schema?: string;
  },
): void {
  emitAddForeignKey({
    table,
    constraint: fkConstraintFromSpec({
      name,
      columns: args.columns,
      references: args.references,
      onDelete: args.onDelete,
      onUpdate: args.onUpdate,
      deferrable: args.deferrable,
      initiallyDeferred: args.initiallyDeferred,
      notValid: args.notValid,
      schema: args.schema,
    }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordAddUnique(
  table: string,
  name: string,
  args: { columns: string[]; ifNotExists?: boolean; schema?: string },
): void {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".unique(name).add needs { columns: string[] }");
  }
  emitAddUnique({
    table,
    constraint: compact({ name, kind: { kind: "unique", columns: args.columns } }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordAddCheck(
  table: string,
  name: string,
  args: { expr: CheckExprFn | PgCheckExprFn; notValid?: boolean; ifNotExists?: boolean; schema?: string },
  checkExprResolver: CheckExprResolver = resolveCoreCheckExpr,
): void {
  if (!args || args.expr === undefined) {
    throw structuredError("OP_INVALID", ".check(name).add needs { expr: (c) => Expr }");
  }
  emitAddCheck({
    table,
    constraint: compact({
      name,
      // `notValid` is PG-only online constraint adoption; compacted out when absent
      // so an ordinary CHECK is byte-identical to the pre-slice wire image.
      kind: compact({
        kind: "check",
        expr: checkExprResolver(args.expr as CheckExprSlot, "check constraint"),
        notValid: args.notValid,
      }),
    }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordValidateConstraint(
  table: string,
  name: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  emitValidateConstraint({
    table,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
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
      wherePredicate: resolveImmutableExpr(
        spec.where as IndexExprFn | ExprChainType | Node | undefined,
        "exclusion predicate",
      ),
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
  if (element && typeof element === "object") {
    if ("column" in element) {
      requireString((element as { column?: unknown }).column, "index column element column");
      const order = indexColumnOrderToIr((element as { order?: unknown }).order);
      // PG-vendor per-column facets: carried through when present, elided when
      // absent (byte-neutral wire shape). Validate gates them fail-closed off PG.
      const opclass = indexElementFacet((element as { opclass?: unknown }).opclass, "index column opclass");
      const collation = indexElementFacet((element as { collation?: unknown }).collation, "index column collation");
      return compact({
        kind: "column",
        name: (element as { column: string }).column,
        order: order === "desc" ? order : undefined,
        opclass,
        collation,
      }) as Node;
    }
    if ("expr" in element) {
      indexColumnOrderToIr((element as { order?: unknown }).order);
      const expr = resolveImmutableExpr(
        (element as { expr?: IndexExprFn | ExprChainType | Node }).expr,
        "index expression element",
      );
      if (!expr) {
        throw structuredError("OP_INVALID", "index expr element needs { expr }");
      }
      return { kind: "expr", expr };
    }
  }
  throw structuredError("OP_INVALID", "index element must be a column name, { column }, or { expr }");
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
  emitAddExclusion({
    table,
    constraint: exclusionConstraintFromSpec({ ...args, name }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordDropConstraint(
  table: string,
  name: string,
  args: { ifExists?: boolean; schema?: string },
): void {
  emitDropConstraint({
    table,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
}

function recordCreateIndex(
  table: string,
  name: string,
  args: PgIndexAdd,
): void {
  if (!Array.isArray(args.on)) {
    throw structuredError("OP_INVALID", ".index(name).add needs { on: IndexElementArg[] }");
  }
  emitCreateIndex({
    table,
    columns: args.on.map(indexElementToIr),
    name,
    unique: args.unique,
    using: args.using,
    where: resolveImmutableExpr(args.where as IndexExprFn | ExprChainType | Node | undefined, "partial index predicate"),
    include: indexIncludeToIr(args.include),
    with: indexWithToIr(args.with),
    only: requireOptionalBoolean(args.only, "index only"),
    nullsNotDistinct: requireOptionalBoolean(args.nullsNotDistinct, "index nullsNotDistinct"),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists),
  });
}

function recordDropIndex(
  table: string,
  name: string,
  args: PgIndexDropArgs,
): void {
  emitDropIndex({
    name,
    table,
    // `unique` drives the destructive/approval gating at apply — preserved here.
    unique: args.unique,
    concurrently: args.concurrently,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
  });
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
  emitInsert({
    table,
    columns: normalized.columns,
    rows: normalized.rows,
    onConflict: normalizeOnConflict(args.onConflict),
    schema: args.schema,
  });
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
  emitUpdate({
    table,
    set: resolveSet(args.set),
    where: resolveExpr(args.where),
    schema: args.schema,
  });
}

function recordDel(table: string, args: DelArgs): void {
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "delete({ where }): where is mandatory (no unfiltered delete)");
  }
  emitDelete({
    table,
    where: resolveExpr(args.where),
    limit: args.limit,
    schema: args.schema,
  });
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

function recordBackfill(table: string, args: BackfillArgs): void {
  if (args.set === undefined) throw structuredError("OP_INVALID", "backfill({ set }): set is required");
  emitBackfill({
    table,
    cursorColumn: args.cursorColumn || DEFAULT_BACKFILL_CURSOR,
    batchSize: args.batchSize !== undefined ? args.batchSize : DEFAULT_BACKFILL_BATCH,
    set: resolveSet(args.set),
    filter: resolveExpr(args.where),
    name: args.name || `backfill_${table}`,
    schema: args.schema,
  });
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

function isRawViewQueryInput(x: unknown): x is { raw: string } {
  return Boolean(x && typeof x === "object" && Object.prototype.hasOwnProperty.call(x, "raw"));
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
    throw structuredError("OP_INVALID", "view(name).create({ as }) requires a structured SelectAst builder or { raw }");
  }
  if (isRawViewQueryInput(args.as)) {
    requireString(args.as.raw, "view(name).create({ as: { raw } })");
    emitCreateView({
      name,
      schema: args.schema,
      columns: args.columns,
      query: { kind: "raw", sql: args.as.raw },
      replace: args.replace,
      materialized: args.materialized,
    });
    return;
  }
  emitCreateView({
    name,
    schema: args.schema,
    columns: args.columns,
    query: { kind: "structured", select: resolveSelectAst(args.as) },
    replace: args.replace,
    materialized: args.materialized,
  });
}

function recordDropView(name: string, args: DropViewArgs & { schema?: string }): void {
  emitDropView({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
    materialized: args.materialized,
  });
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
    delete(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.delete({ table, where, limit?, schema? }) needs an object");
      }
      requireString(args.table, "b.delete({ table })");
      if (args.where === undefined || args.where === null) {
        throw structuredError("OP_INVALID", "b.delete({ where }): where is mandatory (no unfiltered delete)");
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

function resolveTriggerAction(args: TriggerCreateArgs): Node {
  const hasExecute = "execute" in args && args.execute !== undefined;
  const hasBody = "body" in args && args.body !== undefined;
  if (hasExecute === hasBody) {
    throw structuredError(
      "OP_INVALID",
      ".trigger(name).create(...) needs exactly one action: { execute: string } or { body: (b) => TriggerStmt[] }",
    );
  }
  if (hasExecute) {
    requireString(args.execute, ".trigger(name).create({ execute })");
    return { kind: "executeFunction", name: args.execute };
  }
  if (!("body" in args) || typeof args.body !== "function") {
    throw structuredError("OP_INVALID", ".trigger(name).create({ body }) must be a function");
  }
  const statements = args.body(triggerBodyBuilder());
  if (!Array.isArray(statements)) {
    throw structuredError("OP_INVALID", ".trigger(name).create({ body }) must return an array of trigger statements");
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

export function table(name: string, opts: TableOptions = {}): TableHandle {
  return __makeTableHandle(name, opts);
}

export function __makePgTableHandle(name: string, opts: TableOptions = {}): PgTableHandle {
  return __makeTableHandle(name, opts, resolvePgCheckExpr);
}

export function __makeTableHandle(
  name: string,
  opts: TableOptions = {},
  checkExprResolver: CheckExprResolver = resolveCoreCheckExpr,
): PgTableHandle {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;

  const handle: PgTableHandle = {
    // §3.1 — the table itself
    create(args) {
      recordCreateTable(name, { ...args, schema: pickSchema(args, dflt) }, checkExprResolver);
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
      recordSetTableOptions(name, { ...args, schema: dflt });
      return handle;
    },
    comment(text, args = {}) {
      recordComment({ kind: "table", name, schema: pickSchema(args, dflt) }, text);
      return handle;
    },
    partition(partitionName) {
      requireString(partitionName, ".partition(name)");
      const id = registerSelector("partition", partitionName);
      return {
        create(bound, args = {}) {
          terminateSelector(id);
          recordCreatePartition(partitionName, name, partitionBoundToIr(bound), {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        attach(bound, args = {}) {
          terminateSelector(id);
          recordAttachPartition(name, partitionName, partitionBoundToIr(bound), {
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropPartition(name, partitionName, {
            ifExists: args.ifExists,
            cascade: args.cascade,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
        detach(args = {}) {
          terminateSelector(id);
          recordDetachPartition(name, partitionName, {
            concurrently: args.concurrently,
            schema: pickSchema(args, dflt),
          });
          return handle;
        },
      };
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

    // §3.2 — constraints. Selector form is THE grammar (P1: one grammar, one
    // spelling). The `addForeignKey`/`addCheck` verb twins are DELETED — after
    // this slice `foreignKey(name).add`/`check(name).add` are the SOLE public
    // writers of the `addConstraint` fk/check payload (the P1 tier-2 dup —
    // `addConstraint` once had two public writers per slot — is collapsed; the
    // census assertions themselves are a later slice).
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
    check(ckName): PgCheckRef {
      requireString(ckName, ".check(name)");
      const id = registerSelector("check", ckName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddCheck(name, ckName, { ...args, schema: pickSchema(args, dflt) }, checkExprResolver);
          return handle;
        },
      };
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
    constraint(cName): PgConstraintRef {
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
        validate(args = {}) {
          terminateSelector(id);
          recordValidateConstraint(name, cName, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        },
      };
    },

    // §3.4 — indexes
    index(idxName): PgIndexRef {
      requireString(idxName, ".index(name)");
      const id = registerSelector("index", idxName);
      const indexRef: PgIndexRef = {
        add(args) {
          terminateSelector(id);
          recordCreateIndex(name, idxName, { ...args, schema: pickSchema(args, dflt) });
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
    delete(args) {
      recordDel(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },
    backfill(args) {
      recordBackfill(name, { ...args, schema: pickSchema(args, dflt) });
      return handle;
    },

    // `@zeroship/migrate/pg` — table-scoped privileged primitives.
    setRls(args) {
      recordSetRls(name, { ...args, schema: dflt });
      return handle;
    },
    policy(policyName) {
      requireString(policyName, ".policy(name)");
      const id = registerSelector("policy", policyName);
      return {
        create(args) {
          terminateSelector(id);
          if (Array.isArray(args.to) && args.to.length === 0) {
            throw structuredError("OP_INVALID", ".policy(name).create({ to }): to must be a non-empty role array (omit to for PUBLIC)");
          }
          if (args.using === undefined) {
            throw structuredError("OP_INVALID", ".policy(name).create({ using }): using is required (the renderer always emits USING)");
          }
          emitCreatePolicy({
            name: policyName,
            table: name,
            schema: pickSchema(args, dflt),
            forCmd: args.for || "all",
            to: args.to,
            using: resolveExpr(args.using),
            withCheck: resolveExpr(args.withCheck),
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          emitDropPolicy({
            name: policyName,
            table: name,
            schema: pickSchema(args, dflt),
            ifExists: args.ifExists,
          });
          return handle;
        },
      };
    },
    trigger(triggerName) {
      requireString(triggerName, ".trigger(name)");
      const id = registerSelector("trigger", triggerName);
      return {
        create(args) {
          terminateSelector(id);
          emitCreateTrigger({
            name: triggerName,
            table: name,
            schema: pickSchema(args, dflt),
            timing: args.timing,
            events: args.events,
            forEach: args.forEach,
            action: resolveTriggerAction(args),
            when: resolveExpr(args.when),
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          emitDropTrigger({
            name: triggerName,
            table: name,
            schema: pickSchema(args, dflt),
            ifExists: args.ifExists,
          });
          return handle;
        },
      };
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
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "the Date.now symbol (no parens) or now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "the Math.random symbol (no parens) or genRandomUuid()" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "the crypto.randomUUID symbol (no parens) or genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "the Date.now symbol (no parens) or now()" },
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
