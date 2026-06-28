// The `@zeroship/migrate` op-builder DSL — the engine V8 recorder, the LOCK-STEP
// twin of `sdks/migrate/src/ops.ts` (design `2026-06-25-op-dsl-fluent-redesign.md`).
// The Rust runtime `include_str!`s this into V8 to turn a creator's
// `import { table, t } from "@zeroship/migrate"` migration into the canonical
// `.ir.json` the lean engine deserializes.
//
// FLUENT-ONLY: `table()` is the SOLE public entry. The flat op-functions and the
// `e.*` node helper are GONE (pre-launch, no back-compat). Their op-construction
// logic survives as the internal `recordX` helpers the handle delegates to, so the
// emitted wire ops are byte-identical to the pre-redesign flat surface — EXCEPT
// the C1 FK-actions delta. The `.ir.json` shape is frozen and dialect-neutral; the
// golden corpus (`tests/op_fixtures`) + the `Checksum::of_ir` round-trip + the
// variant-exhaustiveness gate are the contract.
//
// CONTRACT: each terminal RECORDS one canonical op object onto the module-local
// recording buffer, synchronously. A migration authors inside `up()`/`down()`; the
// adapter drains the buffer per phase and emits the `.ir.json` envelope. Authoring
// OUTSIDE an active recorder throws a structured `OP_OUTSIDE_RECORDER`. A selector
// that is never terminated throws `SELECTOR_NOT_TERMINATED` at DRAIN (§5).
//
// CHECKSUM: the JS side NEVER computes the checksum; the Rust engine is the single
// `Checksum::of_ir` authority. JS emits ops; Rust folds.
//
// WIRE SHAPE: the op-region fields are camelCase (`ifExists`, `cursorColumn`,
// `batchSize`, `referencesTable`, `onDelete`), matching `op-ir.schema.json`. An
// absent optional is OMITTED (never `field: undefined`/`null`) so the JCS image
// matches the Rust `skip_serializing_if` omitted-key image.

// ---------------------------------------------------------------------------
// The ambient per-migration recorder (§3.1 / §5). The adapter installs a fresh
// recorder before each phase. Authoring OUTSIDE an active recorder is a structured
// error. A handed-out selector that is never terminated is a structured error at
// DRAIN.
// ---------------------------------------------------------------------------

let __active = null;
const __deferredUpOps = [];

// Capture the native nondeterministic function symbols before a migration module
// can mutate globals. A bare symbol is an opt-in to DB-side evaluation; calls just
// evaluate normally and the resulting value records like any other scalar.
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
            "use the crypto.randomUUID symbol (no parens) or c.fn.genRandomUuid()",
        );
      },
      configurable: true,
      writable: false,
    });
  }
}
const __nativeDateNow = typeof Date !== "undefined" ? Date.now : undefined;
const __nativeMathRandom = typeof Math !== "undefined" ? Math.random : undefined;
const __nativeCryptoRandomUUID =
  typeof globalThis !== "undefined" &&
  globalThis.crypto &&
  typeof globalThis.crypto.randomUUID === "function"
    ? globalThis.crypto.randomUUID
    : undefined;

function nativeFnSynthName(value) {
  if (value === __nativeDateNow) return "now";
  if (value === __nativeMathRandom) return "genRandomUuid";
  if (__nativeCryptoRandomUUID !== undefined && value === __nativeCryptoRandomUUID) {
    return "genRandomUuid";
  }
  return undefined;
}

function nativeFnSynthNode(value) {
  const fn = nativeFnSynthName(value);
  return fn === undefined ? undefined : { node: "fnSynth", fn, args: [] };
}

const INVALID_FUNCTION_VALUE_MESSAGE =
  "function values are not valid here; only the supported native symbols " +
  "Date.now / Math.random / crypto.randomUUID translate to DB-evaluated scalars";

/** Structured error helper — mirrors the machine-readable envelope. */
function structuredError(code, message, extra) {
  const err = new Error(message);
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}

function rejectFunctionValue(value) {
  if (typeof value === "function") {
    throw structuredError("OP_INVALID", INVALID_FUNCTION_VALUE_MESSAGE);
  }
}

/** Begin a fresh recording buffer (called by the adapter before a phase). */
export function __begin(phase = "up") {
  __active = {
    ops: phase === "up" ? __deferredUpOps.map((op) => structuredClone(op)) : [],
    pending: new Map(),
    nextSelectorId: 0,
  };
}

/** Drain + return the recorded op list, clearing the active recorder. At DRAIN
 *  (not eagerly — so a var-held selector terminated on a later line is fine),
 *  any selector handed out but never terminated is a hard SELECTOR_NOT_TERMINATED
 *  error. Returns `[]` if no recorder is active. */
export function __drain() {
  if (__active === null) return [];
  const rec = __active;
  __active = null;
  for (const sel of rec.pending.values()) {
    if (!sel.terminated) {
      throw structuredError(
        "SELECTOR_NOT_TERMINATED",
        `selector .${sel.selector}(${JSON.stringify(sel.name)}) was never terminated; ` +
          "a selector records nothing until its terminal (.add/.drop/.rename/.alter) is called",
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

function recorder() {
  if (__active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      "op authoring called outside an active migration recorder; " +
        "the table() handle may only be used synchronously inside up()/down() " +
        "(not at module top level or after the phase returns)",
      {
        suggested_fix:
          "move the table()/selector calls inside the migration's up()/down() body",
      },
    );
  }
  return __active;
}

function push(op) {
  recorder().ops.push(op);
  return op;
}

function pushOrDeferUp(op) {
  if (__active === null) {
    __deferredUpOps.push(op);
    return op;
  }
  __active.ops.push(op);
  return op;
}

/** Register a handed-out selector; returns its id (used at terminate). */
function registerSelector(selector, name) {
  const rec = recorder();
  const id = rec.nextSelectorId++;
  rec.pending.set(id, { selector, name, terminated: false });
  return id;
}

/** Mark a selector terminated; double-terminate is a structured error (§5). */
function terminateSelector(id) {
  const rec = recorder();
  const sel = rec.pending.get(id);
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

/** Drop keys whose value is `undefined` so an absent optional is OMITTED on the
 *  wire (never `"k":null`) — the cross-impl determinism contract. */
function compact(obj) {
  for (const k of Object.keys(obj)) {
    if (obj[k] === undefined) delete obj[k];
  }
  return obj;
}

function requireString(v, what) {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}

function requireStrictness(v, what) {
  if (v === undefined) return undefined;
  if (v !== "strict" && v !== "lenient" && v !== "off") {
    throw structuredError("OP_INVALID", `${what} must be "strict", "lenient", or "off"`);
  }
  return v;
}

function requireOptionalBoolean(v, what) {
  if (v === undefined) return undefined;
  if (typeof v !== "boolean") {
    throw structuredError("OP_INVALID", `${what} must be a boolean`);
  }
  return v;
}

function runtimeOptionsFromCreateArgs(args) {
  const softDelete = requireOptionalBoolean(args.softDelete, "create({ softDelete })");
  const versioning = requireOptionalBoolean(args.versioning, "create({ versioning })");
  const strictness = requireStrictness(args.strictness, "create({ strictness })");
  const hasOptions = softDelete !== undefined || versioning !== undefined || strictness !== undefined;
  if (!hasOptions) return undefined;
  return compact({
    softDelete: softDelete ?? false,
    versioning: versioning ?? false,
    strictness: strictness ?? "strict",
  });
}

function runtimeOptionsPatchFromArgs(args) {
  const softDelete = requireOptionalBoolean(args.softDelete, "setOptions({ softDelete })");
  const versioning = requireOptionalBoolean(args.versioning, "setOptions({ versioning })");
  const strictness = requireStrictness(args.strictness, "setOptions({ strictness })");
  const patch = compact({ softDelete, versioning, strictness });
  if (Object.keys(patch).length === 0) {
    throw structuredError(
      "OP_INVALID",
      "setOptions(...) must set at least one of softDelete, versioning, or strictness",
    );
  }
  return patch;
}

function requireSafeI64(v, what) {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isSafeInteger(v)) {
    throw structuredError("OP_INVALID", `${what} must be a JS safe integer; got ${v}`);
  }
  return v;
}

function requireNullableSafeI64(v, what) {
  if (v === null) return null;
  return requireSafeI64(v, what);
}

function requireSequenceIncrement(v, what) {
  const n = requireSafeI64(v, what);
  if (n === 0) {
    throw structuredError("OP_INVALID", `${what} must be non-zero`);
  }
  return n;
}

function requireSequenceCache(v, what) {
  if (v === undefined) return undefined;
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 1) {
    throw structuredError("OP_INVALID", `${what} must be a positive JS safe integer; got ${v}`);
  }
  return v;
}

function requireSequenceBounds(min, max, what) {
  if (typeof min === "number" && typeof max === "number" && min > max) {
    throw structuredError("OP_INVALID", `${what}: minValue must be <= maxValue`);
  }
}

/** The CLOSED pgvector distance-metric token set (P2a §4) — the camelCase wire
 *  spelling of the Rust `VectorMetric` enum (`cosine | l2 | innerProduct`). Mirrored
 *  here so `t.vector(n, { metric })` rejects an out-of-set metric with a friendly
 *  client-side OP_INVALID (LOW-1); the engine's closed enum stays authoritative. */
const VECTOR_METRICS = ["cosine", "l2", "innerProduct"];

/** The CLOSED column-mask token sets (#174) — the SDK/IR WIRE spelling of the Rust
 *  `IrMaskKind` / `IrClassification` enums. The two date kinds are KEBAB
 *  (`date-year`/`date-decade`) to match the SDK wire form `t.string().mask()` emits;
 *  the rest are single camelCase words. Mirrored here so `.mask({ kind, classification })`
 *  rejects an out-of-set token with a friendly client-side OP_INVALID; the engine's
 *  closed enums stay authoritative. */
const MASK_KINDS = [
  "full",
  "last4",
  "first4",
  "email",
  "name",
  "date-year",
  "date-decade",
  "none",
];
const MASK_CLASSIFICATIONS = ["public", "pii", "spi", "phi", "pci", "internal"];

// ===========================================================================
// (B) The IMMUTABLE chainable `t.*` column-type lexicon (§4). NULLABLE BY
// DEFAULT; `.notNull()` / `.default(x)` / `.ref(target)` / `.primaryKey()` /
// `.unique()` opt in. Each modifier returns a FRESH ColumnDef (no receiver
// mutation), so a hoisted type var is safe to reuse across columns. The
// options-bag overload and the `string`/`int` aliases are REMOVED (§7).
// ===========================================================================

class ColumnDef {
  /** @param {object} colType the dialect-neutral ColType wire value (§4).
   *  @param {object} [fields] nullable/default/primaryKey/unique overrides. */
  constructor(colType, fields) {
    this._type = colType;
    this._nullable = fields && fields.nullable !== undefined ? fields.nullable : true;
    this._default = fields ? fields.default : undefined;
    this._primaryKey = fields && fields.primaryKey !== undefined ? fields.primaryKey : false;
    this._unique = fields && fields.unique !== undefined ? fields.unique : false;
    // Migration-first P2a (§2b): the declared-only, uncatalogable facets carried
    // on the IrColumn — the typed-id prefix (`t.id({prefix})`) and the pgvector
    // distance metric (`t.vector(n, {metric})`). Absent ⇒ omitted on the wire.
    this._idPrefix = fields ? fields.idPrefix : undefined;
    this._vectorMetric = fields ? fields.vectorMetric : undefined;
    // #174: a STANDALONE column mask (`.mask({ kind, classification })`) carried on the
    // IrColumn. Absent ⇒ omitted on the wire. An encrypted column's auto-mask is IMPLIED
    // by `t.encrypted()` (the engine re-derives it) — only an explicit mask lands here.
    this._mask = fields ? fields.mask : undefined;
    this._generated = fields ? fields.generated : undefined;
    this._identity = fields ? fields.identity : undefined;
  }

  /** Clone with the named fields overridden — the basis of immutability (§4). */
  _with(over) {
    return new ColumnDef(over.type !== undefined ? over.type : this._type, {
      nullable: over.nullable !== undefined ? over.nullable : this._nullable,
      default: "default" in over ? over.default : this._default,
      primaryKey: over.primaryKey !== undefined ? over.primaryKey : this._primaryKey,
      unique: over.unique !== undefined ? over.unique : this._unique,
      idPrefix: "idPrefix" in over ? over.idPrefix : this._idPrefix,
      vectorMetric: "vectorMetric" in over ? over.vectorMetric : this._vectorMetric,
      mask: "mask" in over ? over.mask : this._mask,
      generated: "generated" in over ? over.generated : this._generated,
      identity: "identity" in over ? over.identity : this._identity,
    });
  }

  notNull() {
    return this._with({ nullable: false });
  }

  /** `.default(value | { fn: "now" | "genRandomUuid" })` → a structured IrDefault
   *  (typed literal OR nullary synth scalar) — NEVER raw SQL (property A). */
  default(value) {
    return this._with({ default: toIrDefault(value) });
  }

  /** Re-target a column as a foreign-key reference. */
  ref(targetTable) {
    requireString(targetTable, "t.*.ref(target)");
    return this._with({ type: { ref: { references: targetTable } } });
  }

  primaryKey() {
    return this._with({ primaryKey: true, nullable: false });
  }

  unique() {
    return this._with({ unique: true });
  }

  /** `.mask({ kind, classification? })` (#174) — declare a STANDALONE column mask so the
   *  field reads back as `MaskedValue<T>` and the op lower emits the `__zsmask` sentinel
   *  + `_masked` sibling (the same shape `t.encrypted()`'s auto-mask uses). `kind` is
   *  required and one of the closed `MASK_KINDS`; `classification` is optional and
   *  defaults to `"pii"` (the SDK default), one of the closed `MASK_CLASSIFICATIONS`. A
   *  `.mask()` on an ENCRYPTED column is allowed — it OVERRIDES the auto-mask. The
   *  closed-set checks mirror `t.vector(n, { metric })`: a friendly client-side
   *  OP_INVALID over the SAME closed set the engine's enums enforce authoritatively. */
  mask(opts) {
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
    return this._with({ mask: { kind: opts.kind, classification } });
  }

  generated(expr, opts) {
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.generated(expr, opts): opts must be { virtual?: boolean }");
    }
    if (opts && opts.virtual !== undefined && typeof opts.virtual !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.generated(expr, { virtual }): virtual must be a boolean");
    }
    return this._with({
      generated: { expr: resolveExpr(expr), stored: opts && opts.virtual === true ? false : true },
    });
  }

  identity(opts) {
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.identity(opts): opts must be { always?: boolean }");
    }
    if (opts && opts.always !== undefined && typeof opts.always !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.identity({ always }): always must be a boolean");
    }
    return this._with({ identity: { always: opts && opts.always === true ? true : false } });
  }

  /** Reduce to an `IrColumn` (the `createTable` columns[] shape). `name` is the
   *  map key. `nullable`/`default`/`unique` omitted when at their defaults.
   *  C2 — a PRIMARY KEY already IMPLIES uniqueness, so a column that is BOTH
   *  `.unique()` and `.primaryKey()` suppresses the redundant column-level UNIQUE
   *  (lock-step with the addColumn path + the differ). */
  __toIrColumn(name) {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : undefined,
      default: this._default,
      unique: this._unique && !this._primaryKey ? true : undefined,
      // Migration-first P2a (§2b): carry the declared-only facets onto the wire
      // IrColumn so the fold / gen-types (and the runtime under P5) keep the
      // typed-id brand + the vector metric. The wire KEYS are camelCase
      // (`idPrefix` / `vectorMetric`) — the op-region nested-field convention the
      // IrColumn now matches via `#[serde(rename = …)]`, aligning the spelling with
      // the FieldDescriptor + the design §4. The metric VALUE is the closed
      // camelCase token (`cosine | l2 | innerProduct`). Absent ⇒ omitted (compact),
      // so a plain column is byte-identical to the pre-P2a image (checksum-neutral).
      idPrefix: this._idPrefix,
      vectorMetric: this._vectorMetric,
      // #174: carry a STANDALONE mask onto the wire IrColumn (`{ kind, classification }`)
      // so the offline fold + gen-types keep the `MaskedValue<T>` brand and the lower
      // emits the `__zsmask` sentinel. Absent ⇒ omitted (compact), so a mask-less column
      // is byte-identical to the pre-mask image (checksum-neutral).
      mask: this._mask,
      generated: this._generated,
      identity: this._identity,
    });
  }

  /** Reduce to the `addColumn` op tail (`{ type, nullable?, default?, vectorMetric?,
   *  mask? }`).
   *
   *  #173: `Op::AddColumn` NOW carries the `vectorMetric` + `mask` facets (the engine
   *  Op gained the slots), so a vector / masked ADD COLUMN renders the metric opclass /
   *  `__zsmask` sentinel instead of silently dropping the facet. They are carried here.
   *
   *  `idPrefix` STAYS fail-closed: an added column is NEVER the system PK (the table
   *  already has its `id`), so a `t.id({ prefix })` typed-id prefix on an added column is
   *  meaningless — `Op::AddColumn` deliberately has no `idPrefix` slot. A facet-bearing
   *  ColumnDef on an `add({ type })` would otherwise SILENTLY drop the prefix on the wire
   *  (the one outcome the closed-contract discipline forbids); REFUSE it with a structured
   *  OP_INVALID directing the author to declare `t.id({ prefix })` only in create(). */
  __toAddColumnTail() {
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
      // (camelCase keys, lock-step with `Op::AddColumn`). Absent ⇒ omitted (compact),
      // so a plain ADD COLUMN is byte-identical to the pre-#173 wire image.
      vectorMetric: this._vectorMetric,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity,
    });
  }
}

/** Marker the helpers use to tell a fluent `ColumnDef` from a bare ColType. */
function isColumnDef(x) {
  return x instanceof ColumnDef;
}

/** Base64-encode raw bytes (the `IrScalar::Bytes` wire carrier) without a Node
 *  `Buffer` — `btoa` is a WHATWG global present in the V8 record host + Node. */
function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/** Normalize a JS scalar into the closed `IrScalar` WIRE carrier (§3.5):
 *   - a JS `bigint` → `{ decimal: "<v>" }`;
 *   - a `Uint8Array` → `{ bytes: "<base64>" }`;
 *   - everything else passes through verbatim. */
function toIrScalar(value) {
  if (typeof value === "bigint") return { decimal: value.toString() };
  if (typeof value === "number" && Number.isFinite(value) && !Number.isInteger(value)) {
    return { decimal: String(value) };
  }
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  return value;
}

function toIrValue(value) {
  const synth = nativeFnSynthNode(value);
  if (synth !== undefined) return synth;
  rejectFunctionValue(value);
  if (value instanceof ExprChain) return value.__node;
  if (value && typeof value === "object" && typeof value.node === "string") return value;
  return toIrScalar(value);
}

/** Coerce a `.default(value)` arg into the closed `IrDefault` carrier:
 *   - `{ fn: "now" | "genRandomUuid" }` → a nullary synth default;
 *   - any other typed scalar → a `{ literal: { value } }` literal default. */
function toIrDefault(value) {
  const fn = nativeFnSynthName(value);
  if (fn !== undefined) {
    return { fn: { fn } };
  }
  rejectFunctionValue(value);
  if (value && typeof value === "object" && typeof value.fn === "string") {
    return { fn: { fn: value.fn } };
  }
  return { literal: { value: toIrScalar(value) } };
}

/** The immutable fluent column-type lexicon (§4). Canonical names only. */
export const t = {
  /** A conventional primary-key id: a non-null UUID PK defaulting to a DB-evaluated
   *  `gen_random_uuid()` (the structured FnSynth default, never a frozen literal).
   *
   *  `t.id({ prefix })` (P2a §2b) records the declared typed-id prefix on the wire
   *  `IrColumn.idPrefix`, so the fold / gen-types — and the runtime once P5 deletes
   *  the declared-schema cache — keep the `usr_<base62>`-style typed-id brand. The
   *  prefix is bounded at validate-time (charset / length / reserved deny-list);
   *  the recorder records it verbatim (the engine is the authoritative validator). */
  id: (opts) => {
    let col = new ColumnDef("uuid").primaryKey().default({ fn: "genRandomUuid" });
    if (opts && opts.prefix !== undefined) {
      requireString(opts.prefix, "t.id({ prefix })");
      col = col._with({ idPrefix: opts.prefix });
    }
    return col;
  },
  text: () => new ColumnDef("text"),
  /** Fixed-precision decimal. Defaults to (38, 9). */
  numeric: (precision = 38, scale = 9) => new ColumnDef({ decimal: { precision, scale } }),
  timestamp: () => new ColumnDef("timestamp"),
  uuid: () => new ColumnDef("uuid"),
  bytes: () => new ColumnDef("bytea"),
  boolean: () => new ColumnDef("bool"),
  json: () => new ColumnDef("json"),
  /** A foreign-key reference column carrying a plain-string target table name. */
  ref: (targetTable) => {
    requireString(targetTable, "t.ref(target)");
    return new ColumnDef({ ref: { references: targetTable } });
  },
  /** A pgvector embedding column of dimensionality `n`. `t.vector(n, { metric })`
   *  (P2a §2b) records the declared distance metric on the wire
   *  `IrColumn.vectorMetric` (the closed `cosine | l2 | innerProduct` set), so the
   *  ivfflat/hnsw opclass renders the declared metric instead of defaulting — a
   *  DECLARED-ONLY hint DB introspection cannot recover. The metric token is one of
   *  the closed set; the engine REJECTS an out-of-set metric at deserialize. */
  vector: (n, opts) => {
    if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
      throw structuredError("OP_INVALID", `t.vector(n): n must be a positive integer, got ${n}`);
    }
    let col = new ColumnDef({ vector: { vector: n } });
    if (opts && opts.metric !== undefined) {
      requireString(opts.metric, "t.vector(n, { metric })");
      // LOW-1: mirror how `n` is validated client-side — a closed-set check on the
      // metric token gives a friendly OP_INVALID at authoring time instead of a
      // cryptic serde "unknown variant" error at the Rust deserialize seam. The
      // engine remains the authoritative validator (the closed `VectorMetric` enum);
      // this is a redundant, earlier, better-worded guard over the SAME closed set.
      if (!VECTOR_METRICS.includes(opts.metric)) {
        throw structuredError(
          "OP_INVALID",
          `t.vector(n, { metric }): metric must be one of ${VECTOR_METRICS.join(" | ")}, ` +
            `got ${JSON.stringify(opts.metric)}`,
          { metric: opts.metric },
        );
      }
      col = col._with({ vectorMetric: opts.metric });
    }
    return col;
  },
  geoPoint: () => new ColumnDef("geoPoint"),
  /** 32-bit signed integer. */
  integer: () => new ColumnDef("int"),
  int: () => new ColumnDef("int"),
  bigInt: () => new ColumnDef("bigInt"),
  float: () => new ColumnDef("float"),
  enum: (name) => {
    const n = typeof name === "string" ? name : name.name;
    requireString(n, "t.enum(name)");
    return new ColumnDef({ enum: { name: n } });
  },
  domain: (name) => {
    const n = typeof name === "string" ? name : name.name;
    requireString(n, "t.domain(name)");
    return new ColumnDef({ domain: { name: n } });
  },
  /** An application-level encrypted column wrapping an inner `t.*` type. */
  encrypted: (arg) => {
    const inner = arg && arg.of !== undefined ? arg.of : arg;
    const innerType = isColumnDef(inner) ? inner._type : inner;
    if (innerType === undefined) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return new ColumnDef({ encrypted: { of: innerType } });
  },
};

/** Resolve a column-type argument to its ColType wire value (a fluent ColumnDef
 *  or a bare ColType string/object). */
function colTypeOf(typeArg) {
  if (isColumnDef(typeArg)) return typeArg._type;
  return typeArg;
}

export function pgEnum(name, values, args = {}) {
  const enumValues = stringArray(values, "pgEnum(name, values)");
  recordCreateEnum(name, enumValues, args);
  const handle = {
    name,
    values: enumValues,
    create(createArgs = {}) {
      recordCreateEnum(name, enumValues, createArgs);
      return handle;
    },
    drop(dropArgs = {}) {
      recordDropEnum(name, dropArgs);
      return handle;
    },
    comment(text, commentArgs = {}) {
      recordComment({ kind: "type", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

export function pgDomain(name) {
  requireString(name, "pgDomain(name)");
  const handle = {
    name,
    create(args) {
      recordCreateDomain(name, args);
      return handle;
    },
    drop(args = {}) {
      recordDropDomain(name, args);
      return handle;
    },
    comment(text, commentArgs = {}) {
      recordComment({ kind: "type", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

export function sequence(name) {
  requireString(name, "sequence(name)");
  const handle = {
    name,
    create(args = {}) {
      recordCreateSequence(name, args);
      return handle;
    },
    alter(args) {
      recordAlterSequence(name, args);
      return handle;
    },
    drop(args = {}) {
      recordDropSequence(name, args);
      return handle;
    },
    comment(text, commentArgs = {}) {
      recordComment({ kind: "sequence", name, schema: commentArgs.schema }, text);
      return handle;
    },
  };
  return handle;
}

// ===========================================================================
// (B continued) The single-handle fluent `(c) => Expr` builder (§3.6). `c` is
// BOTH a column-accessor function (`c("name")` → a chainable ColRef) and the
// `c.fn.*` scalar-function namespace. `coalesce`/`concatWs` live ONLY on `c.fn`
// (§7 dedup). The chain auto-wraps bare JS values to `Literal`.
// ===========================================================================

function chain(node) {
  return new ExprChain(node);
}

/** Auto-wrap a bare JS value to a `Literal` node; pass a chain/node through. */
function exprArg(x) {
  const synth = nativeFnSynthNode(x);
  if (synth !== undefined) return synth;
  rejectFunctionValue(x);
  if (x instanceof ExprChain) return x.__node;
  if (x && typeof x === "object" && typeof x.node === "string") return x; // a raw AST node
  return { node: "literal", value: x };
}

class ExprChain {
  constructor(node) {
    this.__node = node;
  }

  // ── comparison ──
  eq(x) { return chain({ node: "binOp", op: "eq", lhs: this.__node, rhs: exprArg(x) }); }
  ne(x) { return chain({ node: "binOp", op: "ne", lhs: this.__node, rhs: exprArg(x) }); }
  lt(x) { return chain({ node: "binOp", op: "lt", lhs: this.__node, rhs: exprArg(x) }); }
  le(x) { return chain({ node: "binOp", op: "le", lhs: this.__node, rhs: exprArg(x) }); }
  gt(x) { return chain({ node: "binOp", op: "gt", lhs: this.__node, rhs: exprArg(x) }); }
  ge(x) { return chain({ node: "binOp", op: "ge", lhs: this.__node, rhs: exprArg(x) }); }

  // ── boolean ──
  and(e) { return chain({ node: "binOp", op: "and", lhs: this.__node, rhs: exprArg(e) }); }
  or(e) { return chain({ node: "binOp", op: "or", lhs: this.__node, rhs: exprArg(e) }); }
  not() { return chain({ node: "unaryOp", op: "not", operand: this.__node }); }

  // ── arithmetic ──
  add(x) { return chain({ node: "binOp", op: "add", lhs: this.__node, rhs: exprArg(x) }); }
  sub(x) { return chain({ node: "binOp", op: "sub", lhs: this.__node, rhs: exprArg(x) }); }
  mul(x) { return chain({ node: "binOp", op: "mul", lhs: this.__node, rhs: exprArg(x) }); }
  div(x) { return chain({ node: "binOp", op: "div", lhs: this.__node, rhs: exprArg(x) }); }

  // ── string/value ──
  /** Raw `||` concatenation (NULL-propagating on BOTH backends), folded over the
   *  receiver + every part. For NULL-skipping joins use `c.fn.concatWs` (§3.6). */
  concat(...parts) {
    let acc = this.__node;
    for (const p of parts) {
      acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    }
    return chain(acc);
  }

  // ── null/bool tests ──
  isNull() { return chain({ node: "unaryOp", op: "isNull", operand: this.__node }); }
  isNotNull() { return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node }); }
  isTrue() { return chain({ node: "unaryOp", op: "isTrue", operand: this.__node }); }
  isFalse() { return chain({ node: "unaryOp", op: "isFalse", operand: this.__node }); }

  // ── cast ──
  /** `.cast("integer" | "text" | "real" | "boolean" | "blob")` — the closed
   *  portable target set (§3.6). */
  cast(target) {
    return chain({ node: "cast", operand: this.__node, target });
  }
}

/** Build the single fluent handle `c`: a column-accessor function carrying the
 *  `c.fn.*` namespace. `c("name")` → a chainable ColRef (a plain-string name). */
function makeBuilder() {
  const c = (name) => {
    requireString(name, 'c("name")');
    return chain({ node: "colRef", name });
  };
  c.col = c;
  c.fn = cFn; // the scalar-function namespace (§3.6)
  return c;
}

/** Resolve an expression slot: an `ExprFn` callback `(c) => Expr`, a chainable
 *  `ExprChain`, or a raw closed-AST node object. Returns the closed-AST node. */
function resolveExpr(slot) {
  if (slot === undefined || slot === null) return undefined;
  if (typeof slot === "function") {
    const built = slot(makeBuilder());
    return exprArg(built);
  }
  if (slot instanceof ExprChain) return slot.__node;
  if (slot && typeof slot === "object" && typeof slot.node === "string") return slot;
  throw structuredError(
    "OP_INVALID",
    "expression slot must be a (c) => Expr callback or a built expression",
  );
}

/** Resolve a `set: { col: ExprFn }` map into a `{ col: node }` wire map. */
function resolveSet(set) {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column → expression");
  }
  const out = {};
  for (const col of Object.keys(set)) {
    out[col] = resolveExpr(set[col]);
  }
  return out;
}

// ===========================================================================
// (B continued) `c.fn.*` — the scalar-function namespace (§3.6). Reached off the
// single builder handle (no importable `fn`). `coalesce`/`concatWs` live ONLY
// here (§7 dedup). Each member builds exactly one closed-AST node.
// ===========================================================================

export const cFn = {
  lower: (e) => chain({ node: "fnCall", fn: "lower", args: [exprArg(e)] }),
  upper: (e) => chain({ node: "fnCall", fn: "upper", args: [exprArg(e)] }),
  trim: (e) => chain({ node: "fnCall", fn: "trim", args: [exprArg(e)] }),
  length: (e) => chain({ node: "fnCall", fn: "length", args: [exprArg(e)] }),
  abs: (e) => chain({ node: "fnCall", fn: "abs", args: [exprArg(e)] }),
  coalesce: (...args) => chain({ node: "fnCall", fn: "coalesce", args: args.map(exprArg) }),
  nullif: (a, b) => chain({ node: "fnCall", fn: "nullif", args: [exprArg(a), exprArg(b)] }),

  // VENDOR (`@zeroship/migrate/pg`) scalars (vendor spec §2.10) — the GUC + identity
  // functions the RLS policy / trigger predicates need (`0025`'s
  // `current_setting('zeroship.tenant_app', true)`). Closed `fnCall` nodes, NOT a raw
  // escape; PG-only (the containing vendor op is PgOnly). `currentSetting(name,
  // missingOk?)` → `current_setting('…', <missingOk>)`; `currentUser()` → `current_user`.
  currentSetting: (name, missingOk) =>
    chain({
      node: "fnCall",
      fn: "currentSetting",
      args: missingOk === undefined
        ? [{ node: "literal", value: name }]
        : [{ node: "literal", value: name }, { node: "literal", value: missingOk }],
    }),
  currentUser: () => chain({ node: "fnCall", fn: "currentUser", args: [] }),

  /** NULL-skipping `concat_ws` (PG) / `coalesce`-folded `||` (SQLite) — the safe
   *  join helper (§3.6). `sep` is a literal. */
  concatWs: (sep, ...parts) =>
    chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] }),

  /** The searched `CASE` form (`c.fn.case([[cond, val], …], elseVal)`). */
  case: (branches, elseVal) => {
    if (!Array.isArray(branches)) {
      throw structuredError("OP_INVALID", "c.fn.case(branches, else?): branches must be an array of [cond, result]");
    }
    const node = {
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

  /** The engine-synthesized portable split helper (§9). `delim` is a string
   *  literal; `n` a positive integer literal. */
  splitPart: (col, delim, n) => {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [exprArg(col), { node: "literal", value: delim }, { node: "literal", value: n }],
    });
  },

  /** DB-evaluated apply-time scalars, equivalent to the supported bare native
   *  symbols (`Date.now`, `Math.random`, `crypto.randomUUID`). */
  now: () => chain({ node: "fnSynth", fn: "now", args: [] }),
  genRandomUuid: () => chain({ node: "fnSynth", fn: "genRandomUuid", args: [] }),
};

// ===========================================================================
// (C) Existence-guard token mappers. The create/add family takes `ifNotExists`;
// the drop/rename/alter family takes `ifExists`. Engine-synthesized via a catalog
// probe — NOT a native `IF [NOT] EXISTS` clause.
// ===========================================================================

function ifNotExistsGuard(v) {
  return v ? "ifNotExists" : undefined;
}
function ifExistsGuard(v) {
  return v ? "ifExists" : undefined;
}

function stringArray(values, what) {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a string[]`);
  }
  for (const v of values) requireString(v, what);
  return [...values];
}

function recordCreateEnum(name, values, args = {}) {
  requireString(name, "pgEnum(name, values)");
  pushOrDeferUp(
    compact({
      op: "createEnum",
      name,
      schema: args.schema,
      values: stringArray(values, "pgEnum(name, values)"),
    }),
  );
}

function recordDropEnum(name, args = {}) {
  requireString(name, "pgEnum(name, values).drop()");
  push(
    compact({
      op: "dropEnum",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateDomain(name, args) {
  requireString(name, "pgDomain(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "pgDomain(name).create({ as, ... }) needs an object");
  }
  if (args.notNull !== undefined && typeof args.notNull !== "boolean") {
    throw structuredError("OP_INVALID", "pgDomain(name).create({ notNull }): notNull must be a boolean");
  }
  pushOrDeferUp(
    compact({
      op: "createDomain",
      name,
      schema: args.schema,
      as: colTypeOf(args.as),
      check: resolveExpr(args.check),
      default: args.default === undefined ? undefined : toIrDefault(args.default),
      notNull: args.notNull,
    }),
  );
}

function recordDropDomain(name, args = {}) {
  requireString(name, "pgDomain(name).drop()");
  push(
    compact({
      op: "dropDomain",
      name,
      schema: args.schema,
      existenceGuard: ifExistsGuard(args.ifExists),
    }),
  );
}

function recordCreateSequence(name, args = {}) {
  requireString(name, "sequence(name)");
  if (args === null || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).create(args) needs an object");
  }
  const minValue = requireNullableSafeI64(args.minValue, "sequence.create({ minValue })");
  const maxValue = requireNullableSafeI64(args.maxValue, "sequence.create({ maxValue })");
  requireSequenceBounds(minValue, maxValue, "sequence.create(args)");
  pushOrDeferUp(
    compact({
      op: "createSequence",
      name,
      schema: args.schema,
      as: args.as === undefined ? undefined : colTypeOf(args.as),
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

function recordAlterSequence(name, args) {
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

function recordDropSequence(name, args = {}) {
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

function recordComment(target, text) {
  if (text !== null && typeof text !== "string") {
    throw structuredError("OP_INVALID", "comment text must be a string or null");
  }
  push(
    compact({
      op: "comment",
      target: commentTargetToIr(target),
      comment: text,
    }),
  );
}

function commentTargetToIr(target) {
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
      throw structuredError("OP_INVALID", `unsupported comment target kind ${target.kind}`);
  }
}

// ===========================================================================
// (D) The internal op-construction helpers (the single source of truth). These
// build + push the EXACT canonical op object the Rust closed `Op` enum /
// `op-ir.schema.json` deserialize (byte-identical to the pre-redesign flat
// surface except the C1 FK-actions delta). Only the fluent `table()` handle calls
// them.
// ===========================================================================

function recordCreateTable(name, args) {
  const cols = [];
  const constraints = [];
  const indexes = [];
  const pkCols = [];

  const columns = args.columns || {};
  for (const colName of Object.keys(columns)) {
    const def = columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError(
        "OP_INVALID",
        `create column "${colName}" must be a t.* ColumnDef (got ${typeof def})`,
      );
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  // A composite `primaryKey: [...]` wins over per-column `.primaryKey()` hoists.
  const pk = Array.isArray(args.primaryKey) && args.primaryKey.length > 0 ? args.primaryKey : pkCols;
  if (pk.length > 0) constraints.push({ kind: { kind: "pk", columns: pk } });

  for (const uq of args.uniques || []) {
    constraints.push(compact({ name: uq.name, kind: { kind: "unique", columns: uq.columns } }));
  }
  for (const ck of args.checks || []) {
    constraints.push(compact({ name: ck.name, kind: { kind: "check", expr: resolveExpr(ck.expr) } }));
  }
  for (const exclusion of args.exclusions || []) {
    constraints.push(exclusionConstraintFromSpec(exclusion));
  }
  for (const fkSpec of args.foreignKeys || []) {
    constraints.push(
      fkConstraintFromSpec({
        name: fkSpec.name,
        columns: fkSpec.columns,
        references: fkSpec.references,
        onDelete: fkSpec.onDelete,
        onUpdate: fkSpec.onUpdate,
      }),
    );
  }
  for (const idx of args.indexes || []) {
    indexes.push(
      compact({
        name: idx.name,
        columns: idx.columns.map(indexElementToIr),
        unique: idx.unique,
        using: idx.using,
        where: resolveExpr(idx.where),
      }),
    );
  }

  push(
    compact({
      op: "createTable",
      name,
      columns: cols,
      constraints: constraints.length ? constraints : undefined,
      indexes: indexes.length ? indexes : undefined,
      runtimeOptions: runtimeOptionsFromCreateArgs(args),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordSetTableOptions(table, args) {
  push(
    compact({
      op: "setTableOptions",
      table,
      options: runtimeOptionsPatchFromArgs(args),
      schema: args.schema,
    }),
  );
}

function recordDropTable(table, args) {
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

function recordRenameTable(table, to, args) {
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

function recordAddColumn(table, column, type, args) {
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
  // C2 — `.column(x).add({ type: t.text().unique() })` honors `.unique()`: an
  // ADD COLUMN has no inline UNIQUE, so it lowers to a separate ADD CONSTRAINT.
  // Likewise `.primaryKey()` hoists a pk add. A PRIMARY KEY already IMPLIES
  // uniqueness, so when BOTH are set the follow-on UNIQUE is redundant DDL —
  // suppress it (lock-step with the TS surface + the differ, which never emits a
  // separate UNIQUE for the PK column). Only the pk add is recorded.
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

function recordDropColumn(table, column, args) {
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

function recordRenameColumn(table, from, to, type, args) {
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

function recordAlterColumn(table, name, change) {
  if (change.type !== undefined) {
    push(
      compact({
        op: "alterColumnType",
        table,
        column: name,
        type: colTypeOf(change.type),
        using: resolveExpr(change.using),
        schema: change.schema,
      }),
    );
    return;
  }
  if (change.nullable !== undefined) {
    push(
      compact({
        op: "alterColumnNullability",
        table,
        column: name,
        nullable: change.nullable,
        schema: change.schema,
      }),
    );
    return;
  }
  throw structuredError("OP_INVALID", ".column(name).alter({…}) must carry `type` or `nullable`");
}

/** Build an `IrConstraint` of kind `fk`. **C1**: `onDelete`/`onUpdate` ARE
 *  emitted (compacted — omitted when absent, so an action-free FK is byte-
 *  identical to the pre-C1 wire image). */
function fkConstraintFromSpec(spec) {
  if (!spec || typeof spec !== "object" || !spec.references) {
    throw structuredError("OP_INVALID", ".foreignKey(name).add needs { columns, references:{ table, columns } }");
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

function recordAddForeignKey(table, name, args) {
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
      }),
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordAddUnique(table, name, args) {
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

function recordAddCheck(table, name, args) {
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

function exclusionConstraintFromSpec(spec) {
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
      wherePredicate: resolveExpr(spec.where),
      deferrable: spec.deferrable,
      initiallyDeferred: spec.initiallyDeferred,
    }),
  });
}

function exclusionElementToIr(element) {
  if (!element || typeof element !== "object") {
    throw structuredError("OP_INVALID", "exclusion element must be { target, operator }");
  }
  return {
    target: exclusionTargetToIr(element.target),
    operator: element.operator,
  };
}

function exclusionTargetToIr(target) {
  if (typeof target === "string") {
    requireString(target, "exclusion target column");
    return { kind: "column", name: target };
  }
  const expr = resolveExpr(target);
  if (!expr) {
    throw structuredError("OP_INVALID", "exclusion target must be a column name or expression");
  }
  return { kind: "expr", expr };
}

function indexElementToIr(element) {
  if (typeof element === "string") {
    requireString(element, "index element column");
    return { kind: "column", name: element };
  }
  if (element && typeof element === "object" && "kind" in element) {
    if (element.kind === "column") {
      requireString(element.name, "index column element name");
      return { kind: "column", name: element.name };
    }
    if (element.kind === "expr") {
      const expr = resolveExpr(element.expr);
      if (!expr) {
        throw structuredError("OP_INVALID", "index expr element needs { kind: \"expr\", expr }");
      }
      return { kind: "expr", expr };
    }
  }
  const expr = resolveExpr(element);
  if (!expr) {
    throw structuredError("OP_INVALID", "index element must be a column name or expression");
  }
  return { kind: "expr", expr };
}

function recordAddExclusion(table, name, args) {
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

function recordDropConstraint(table, name, args) {
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

function recordCreateIndex(table, name, args) {
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
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists),
    }),
  );
}

function recordDropIndex(table, name, args) {
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

function normalizeInsertRows(rows, what) {
  let normalizedRows = rows;
  if (normalizedRows === undefined) {
    throw structuredError("OP_INVALID", `${what}: rows is required`);
  }
  if (!Array.isArray(normalizedRows)) normalizedRows = [normalizedRows];
  const columns = normalizedRows.length > 0 ? Object.keys(normalizedRows[0]) : [];
  const positional = normalizedRows.map((r) =>
    columns.map((col) => (Object.prototype.hasOwnProperty.call(r, col) ? toIrValue(r[col]) : null)),
  );
  return { columns, rows: positional };
}

function recordInsert(table, args) {
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

/** Normalize an `onConflict.doUpdate` `column → scalar` map through the IrScalar
 *  carrier so a bigint/Uint8Array assignment matches the Rust `IrOnConflict`
 *  `BTreeMap<String, IrScalar>` shape. */
function normalizeOnConflict(oc) {
  if (oc === undefined || oc === null) return undefined;
  if (oc.doUpdate === undefined) return { columns: oc.columns };
  const doUpdate = {};
  for (const col of Object.keys(oc.doUpdate)) doUpdate[col] = toIrValue(oc.doUpdate[col]);
  return { columns: oc.columns, doUpdate };
}

function recordUpdate(table, args) {
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

function recordDel(table, args) {
  if (args.where === undefined || args.where === null) {
    throw structuredError("OP_INVALID", "del({ where }): where is mandatory (no unfiltered delete)");
  }
  push(compact({ op: "delete", table, where: resolveExpr(args.where), limit: args.limit, schema: args.schema }));
}

const DEFAULT_BACKFILL_CURSOR = "id";
const DEFAULT_BACKFILL_BATCH = 1000;

function recordBackfill(table, args) {
  if (args.set === undefined) {
    throw structuredError("OP_INVALID", "backfill({ set }): set is required");
  }
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

function normalizeTableRef(input, what) {
  if (typeof input === "string") return { name: input };
  if (!input || typeof input !== "object") {
    throw structuredError("OP_INVALID", `${what} must be a table name string or { name, schema?, alias? }`);
  }
  requireString(input.name, `${what}.name`);
  if (input.schema !== undefined) requireString(input.schema, `${what}.schema`);
  if (input.alias !== undefined) requireString(input.alias, `${what}.alias`);
  return compact({ name: input.name, schema: input.schema, alias: input.alias });
}

function normalizeSelectItem(item) {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChain) {
    return { kind: "expr", expr: resolveExpr(item) };
  }
  if (item && typeof item === "object") {
    if (item.node !== undefined) return { kind: "expr", expr: resolveExpr(item) };
    if (item.kind === "colRef") {
      requireString(item.name, "select item colRef.name");
      if (item.table !== undefined) requireString(item.table, "select item colRef.table");
      if (item.alias !== undefined) requireString(item.alias, "select item colRef.alias");
      return compact({ kind: "colRef", table: item.table, name: item.name, alias: item.alias });
    }
    if (item.kind === "expr") {
      if (item.alias !== undefined) requireString(item.alias, "select item expr.alias");
      return compact({ kind: "expr", expr: resolveExpr(item.expr), alias: item.alias });
    }
  }
  throw structuredError("OP_INVALID", "select item must be a column name, expression, or SelectItem object");
}

function normalizeOrderItem(item) {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChain) {
    return { kind: "expr", expr: resolveExpr(item) };
  }
  if (item && typeof item === "object") {
    if (item.node !== undefined) return { kind: "expr", expr: resolveExpr(item) };
    if (item.kind === "colRef") {
      requireString(item.name, "order item colRef.name");
      if (item.table !== undefined) requireString(item.table, "order item colRef.table");
      return compact({ kind: "colRef", table: item.table, name: item.name, dir: item.dir });
    }
    if (item.kind === "expr") {
      return compact({ kind: "expr", expr: resolveExpr(item.expr), dir: item.dir });
    }
  }
  throw structuredError("OP_INVALID", "orderBy item must be a column name, expression, or OrderItem object");
}

function viewQueryBuilder() {
  const state = {
    from: undefined,
    projection: [],
    joins: [],
    where: undefined,
    orderBy: undefined,
    limit: undefined,
  };
  const builder = {
    from(table) {
      state.from = normalizeTableRef(table, "view query from(table)");
      return builder;
    },
    select(items) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query select(items): items must be an array");
      }
      state.projection = items.map(normalizeSelectItem);
      return builder;
    },
    join(kind, table, on) {
      if (kind !== "inner" && kind !== "left") {
        throw structuredError("OP_INVALID", "view query join(kind): kind must be inner or left");
      }
      state.joins.push({
        kind,
        table: normalizeTableRef(table, "view query join(table)"),
        on: resolveExpr(on),
      });
      return builder;
    },
    innerJoin(table, on) {
      return builder.join("inner", table, on);
    },
    leftJoin(table, on) {
      return builder.join("left", table, on);
    },
    where(expr) {
      state.where = resolveExpr(expr);
      return builder;
    },
    orderBy(items) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query orderBy(items): items must be an array");
      }
      state.orderBy = items.map(normalizeOrderItem);
      return builder;
    },
    limit(n) {
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
      });
    },
  };
  return builder;
}

function resolveSelectAst(as) {
  if (typeof as === "function") {
    const q = viewQueryBuilder();
    const built = as(q) || q;
    if (built && typeof built.__selectAst === "function") return built.__selectAst();
    if (built && typeof built === "object" && built.from !== undefined) return built;
  }
  if (as && typeof as === "object" && typeof as.__selectAst === "function") return as.__selectAst();
  if (as && typeof as === "object" && as.from !== undefined) return as;
  throw structuredError("OP_INVALID", "view.create({ as }) must be a query-builder callback or SelectAst");
}

function recordCreateView(name, args) {
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

function recordCreateRawView(name, args) {
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

function recordDropView(name, args) {
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

const TRIGGER_RAISE_LEVELS = ["abort", "fail", "ignore", "rollback"];

function triggerBodyBuilder() {
  return {
    raise(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.raise({ level, message, errcode? }) needs an object");
      }
      requireString(args.level, "b.raise({ level })");
      if (!TRIGGER_RAISE_LEVELS.includes(args.level)) {
        throw structuredError(
          "OP_INVALID",
          `b.raise({ level }): level must be one of ${TRIGGER_RAISE_LEVELS.join(" | ")}, got ${JSON.stringify(args.level)}`,
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
      });
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
      });
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
      });
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
      });
    },
    select(expr) {
      return { stmt: "select", expr: resolveExpr(expr) };
    },
  };
}

function resolveTriggerAction(args) {
  const hasExecute = args.execute !== undefined;
  const hasBody = args.body !== undefined;
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
  if (typeof args.body !== "function") {
    throw structuredError("OP_INVALID", ".createTrigger({ body }) must be a function");
  }
  const statements = args.body(triggerBodyBuilder());
  if (!Array.isArray(statements)) {
    throw structuredError("OP_INVALID", ".createTrigger({ body }) must return an array of trigger statements");
  }
  for (const stmt of statements) {
    if (!stmt || typeof stmt !== "object" || typeof stmt.stmt !== "string") {
      throw structuredError("OP_INVALID", "trigger body entries must be statements returned by the trigger body builder");
    }
  }
  return { kind: "body", statements };
}

// ===========================================================================
// (E) The fluent `table()` handle — the SOLE public entry (§3). The byte-for-byte
// twin of `sdks/migrate/src/ops.ts`'s `table()`. A reusable value carrying only
// `{ name, schemaDefault }`; terminals record EAGERLY and return the handle, so it
// is valid for unlimited chaining + var-reuse (§4). A per-op `schema` overrides
// the table default by key presence.
// ===========================================================================

/** Per-op-wins-over-table-default schema precedence (§3/§4). */
function pickSchema(perCall, dflt) {
  if (perCall && perCall.schema !== undefined) return perCall.schema;
  return dflt;
}

function pickViewColumns(perCall, dflt) {
  if (perCall && perCall.columns !== undefined) return perCall.columns;
  return dflt;
}

function requireColumnDef(x, where) {
  if (!isColumnDef(x)) {
    throw structuredError("OP_INVALID", `${where} must be a t.* ColumnDef`);
  }
}

export function comment(target, text) {
  recordComment(target, text);
}

export function table(name, opts = {}) {
  requireString(name, "table(name, …)");
  const dflt = opts.schema;

  const handle = {
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

    // §3.2 — columns
    column(col) {
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
        alter(args) {
          terminateSelector(id);
          recordAlterColumn(name, col, { ...args, schema: pickSchema(args, dflt) });
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
    foreignKey(fkName) {
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
    unique(uqName) {
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
    check(ckName) {
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
    exclusion(exName) {
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
    constraint(cName) {
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
    index(idxName) {
      requireString(idxName, ".index(name)");
      const id = registerSelector("index", idxName);
      return {
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

    // ── VENDOR (`@zeroship/migrate/pg`) — table-scoped privileged primitives ──
    // RLS / policies hang off the table handle (vendor spec §2.4/§2.5).
    // Exposed always; the engine's capability gate refuses them fail-closed under
    // a confined capability set. Each pushes a vendor op carrying the table.
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
    createPolicy(args) {
      requireString(args.name, ".createPolicy({ name })");
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
    dropPolicy(args) {
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

export function view(name, opts = {}) {
  requireString(name, "view(name, …)");
  const dflt = opts.schema;
  const dfltColumns = opts.columns;

  const handle = {
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

// ===========================================================================
// VENDOR (`@zeroship/migrate/pg`) — the standalone `pg.*` namespace for the
// database-/role-/schema-level privileged primitives (vendor spec §2.1–2.6,
// §2.11). These have no table handle to hang off. Each eagerly records a vendor
// op onto the ambient recorder, byte-identically to the Rust `Op` wire shape
// (internally-tagged camelCase; absent optionals OMITTED via `compact`). The
// engine's capability gate refuses every one fail-closed under a confined
// capability set; the rendered SQL is then deny-list-scanned at lower.
// ===========================================================================
export const pg = {
  createSchema(args) {
    requireString(args.name, "pg.createSchema({ name })");
    return push(compact({
      op: "createSchema",
      name: args.name,
      ifNotExists: args.ifNotExists,
      authorization: args.authorization,
    }));
  },
  dropSchema(args) {
    requireString(args.name, "pg.dropSchema({ name })");
    return push(compact({
      op: "dropSchema",
      name: args.name,
      ifExists: args.ifExists,
      cascade: args.cascade,
    }));
  },
  createExtension(args) {
    requireString(args.name, "pg.createExtension({ name })");
    return push(compact({
      op: "createExtension",
      name: args.name,
      ifNotExists: args.ifNotExists,
      schema: args.schema,
    }));
  },
  dropExtension(args) {
    requireString(args.name, "pg.dropExtension({ name })");
    return push(compact({ op: "dropExtension", name: args.name, ifExists: args.ifExists }));
  },
  createRole(args) {
    requireString(args.name, "pg.createRole({ name })");
    return push(compact({
      op: "createRole",
      name: args.name,
      login: args.login,
      password: args.password,
      bypassRls: args.bypassRls,
      createRole: args.createRole,
      createDb: args.createDb,
      superuser: args.superuser,
      inRole: args.inRole,
      setSearchPath: args.setSearchPath,
      ifNotExists: args.ifNotExists,
    }));
  },
  alterRole(args) {
    requireString(args.name, "pg.alterRole({ name })");
    return push(compact({
      op: "alterRole",
      name: args.name,
      setSearchPath: args.setSearchPath,
      resetSearchPath: args.resetSearchPath,
    }));
  },
  dropRole(args) {
    requireString(args.name, "pg.dropRole({ name })");
    return push(compact({ op: "dropRole", name: args.name, ifExists: args.ifExists }));
  },
  dropOwnedBy(args) {
    if (!Array.isArray(args.roles)) {
      throw structuredError("OP_INVALID", "pg.dropOwnedBy({ roles }): roles must be an array");
    }
    return push(compact({ op: "dropOwnedBy", roles: args.roles }));
  },
  grant(args) {
    return push(compact({
      op: "grant",
      privileges: args.privileges,
      on: args.on,
      to: args.to,
      withGrantOption: args.withGrantOption,
    }));
  },
  revoke(args) {
    return push(compact({
      op: "revoke",
      privileges: args.privileges,
      on: args.on,
      from: args.from,
    }));
  },
  createFunction(args) {
    requireString(args.name, "pg.createFunction({ name })");
    requireString(args.returns, "pg.createFunction({ returns })");
    requireString(args.language, "pg.createFunction({ language })");
    requireString(args.body, "pg.createFunction({ body })");
    return push(compact({
      op: "createFunction",
      name: args.name,
      schema: args.schema,
      args: args.args,
      returns: args.returns,
      language: args.language,
      replace: args.replace,
      volatility: args.volatility,
      body: args.body,
    }));
  },
  dropFunction(args) {
    requireString(args.name, "pg.dropFunction({ name })");
    return push(compact({
      op: "dropFunction",
      name: args.name,
      schema: args.schema,
      argTypes: args.argTypes,
      ifExists: args.ifExists,
    }));
  },
  /** The gated raw-statement escape (`pg.sql\`…\``, vendor spec §2.11). A tagged
   *  template whose interpolation slots accept ONLY typed binds (never identifiers
   *  / SQL) — the binds become positional placeholders, the verbatim text is
   *  embedded and `pg_query`-scanned by the guard at lower. */
  sql(strings, ...binds) {
    // Reassemble the template into a single statement, replacing each interpolation
    // slot with a positional placeholder ($1, $2, …) so a bind can never be string-
    // concatenated into the statement shape.
    let out = strings[0];
    for (let i = 0; i < binds.length; i++) {
      out += `$${i + 1}` + strings[i + 1];
    }
    return push(compact({
      op: "pgRaw",
      sql: out,
      binds: binds.length > 0 ? binds.map(scalarBind) : undefined,
    }));
  },
};

/** Coerce a `pg.sql` bind into the IR scalar wire form. Numbers/strings/bools pass
 *  through; everything else is rejected fail-closed (a `pg.sql` bind is a typed
 *  scalar, never an object/identifier). */
function scalarBind(v) {
  const t = typeof v;
  if (t === "string" || t === "boolean") return v;
  if (t === "number") {
    if (!Number.isInteger(v)) {
      throw structuredError("OP_INVALID", `pg.sql bind ${v} must be an integer scalar (use a decimal string for non-integers)`);
    }
    return v;
  }
  throw structuredError("OP_INVALID", `pg.sql bind must be a typed scalar (string/number/boolean); got ${t}`);
}

// ===========================================================================
// (C) Determinism lint. Flag CALLS to JS nondeterminism accessors (`Date.now()` /
// `Math.random()` / `crypto.randomUUID()` / `new Date()`), steering authors to the
// bare function SYMBOL or the DB-evaluated `c.fn.*` (`FnSynth`) scalar. This is a
// coarse AST-free SOURCE scan and is advisory only.
// ===========================================================================

const NONDETERMINISM_PATTERNS = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "the Date.now symbol (no parens) or c.fn.now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "the Math.random symbol (no parens) or c.fn.genRandomUuid()" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "the crypto.randomUUID symbol (no parens) or c.fn.genRandomUuid()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "the Date.now symbol (no parens) or c.fn.now()" },
];

/**
 * Lint a migration's SOURCE TEXT for the nondeterminism accessors. Returns an
 * array of `{ code, accessor, suggested_fix, reason }` findings (empty ⇒ clean).
 *
 * SCOPE — intentional coarse whole-source scan: it OVER-flags (a clock accessor
 * in a comment / a non-op helper trips it) and NEVER under-flags. The record/build
 * path surfaces these only as warnings; calls are allowed and record their
 * evaluated value.
 */
export function lintDeterminism(source) {
  if (typeof source !== "string") return [];
  const findings = [];
  for (const { re, name, steer } of NONDETERMINISM_PATTERNS) {
    if (re.test(source)) {
      findings.push({
        code: "NONDETERMINISTIC_OP_ARG",
        accessor: name,
        suggested_fix: `replace ${name} with the DB-evaluated ${steer}`,
        reason:
          `${name} bakes a build-time value into the migration artifact; for a value that ` +
          "must be computed at apply time use the structured FnSynth scalar",
      });
    }
  }
  return findings;
}

// ---------------------------------------------------------------------------
// `c.fn.splitPart` grammar lint (§9) — the dialect-NEUTRAL subset broken on BOTH
// backends (a non-string/empty delimiter, a non-positive-int n). A violation
// throws a structured EXPR_NOT_PORTABLE error. The portability ENVELOPE
// (single-ASCII delimiter, 1<=n<=8) is dialect-gated and deferred to the Rust
// validator.
// ---------------------------------------------------------------------------

function splitPartGrammarLint(delim, n) {
  const fail = (reason) => {
    throw structuredError("EXPR_NOT_PORTABLE", reason, {
      suggested_fix:
        "pass a non-empty string-literal delimiter and a positive-integer n; to target " +
        "SQLite too, stay in-envelope (single-ASCII delimiter, 1<=n<=8) — a multi-char/" +
        "non-ASCII delimiter or n>8 renders only on Postgres (dialect_scope=PgOnly)",
    });
  };
  if (typeof delim !== "string") {
    fail(`c.fn.splitPart delimiter must be a string literal; got ${typeof delim}`);
  }
  if (delim.length === 0) {
    fail("c.fn.splitPart delimiter must be a non-empty string literal");
  }
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`c.fn.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if (n < 1) {
    fail(`c.fn.splitPart part index n must be a positive integer; got ${n}`);
  }
}
