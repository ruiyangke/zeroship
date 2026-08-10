// src/ops.ts
if (typeof globalThis !== "undefined") {
  if (typeof globalThis.crypto === "undefined" || globalThis.crypto === null) {
    Object.defineProperty(globalThis, "crypto", {
      value: {},
      configurable: true,
      writable: false
    });
  }
  if (typeof globalThis.crypto.randomUUID !== "function") {
    Object.defineProperty(globalThis.crypto, "randomUUID", {
      value: function randomUUID() {
        throw new Error(
          "crypto.randomUUID() is not available in the migration recorder; use the crypto.randomUUID symbol (no parens) or uuidV4()"
        );
      },
      configurable: true,
      writable: false
    });
  }
}
var nativeDateNow = typeof Date !== "undefined" ? Date.now : void 0;
var nativeMathRandom = typeof Math !== "undefined" ? Math.random : void 0;
var nativeCryptoRandomUUID = typeof globalThis.crypto !== "undefined" && typeof globalThis.crypto.randomUUID === "function" ? globalThis.crypto.randomUUID : void 0;
var NEXTVAL_DEFAULT_MARKER = "__zeroMigrateNextvalDefault";
var INT64_VALUE_BRAND = /* @__PURE__ */ Symbol.for("zero-migrate.int64/v1");
var DECIMAL_VALUE_BRAND = /* @__PURE__ */ Symbol.for("zero-migrate.decimal/v1");
var BYTES_VALUE_BRAND = /* @__PURE__ */ Symbol.for("zero-migrate.bytes/v1");
var PER_ROW_GENERATOR_BRAND = /* @__PURE__ */ Symbol.for("zero-migrate.perRowGenerator/v1");
var INT64_STRING_RE = /^(?:0|-?[1-9][0-9]*)$/;
var INT64_MIN = -(1n << 63n);
var INT64_MAX = (1n << 63n) - 1n;
var DECIMAL_STRING_RE = /^-?\d+(?:\.\d+)?$/;
var BASE64_STRING_RE = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}(?:==)?|[A-Za-z0-9+/]{3}=?)?$/;
var INT64_VALUE_ERROR = 'int64(value) requires a canonical signed integer bigint or decimal string (for example int64(42n) or int64("42"))';
var INT64_RANGE_ERROR = "int64(value) must be between -9223372036854775808 and 9223372036854775807";
var DECIMAL_VALUE_ERROR = 'decimal(value) requires a well-formed decimal string; use decimal("<n>") or decimal("0.00")';
var BYTES_VALUE_ERROR = 'byteValue(bytes) requires a Uint8Array or well-formed base64 string; use byteValue(new Uint8Array([...])) or byteValue("<base64>")';
function requireInt64String(value) {
  if (typeof value !== "bigint" && typeof value !== "string") {
    throw structuredError("OP_INVALID", INT64_VALUE_ERROR);
  }
  const rendered = typeof value === "bigint" ? String(value) : value;
  if (!INT64_STRING_RE.test(rendered)) {
    throw structuredError("OP_INVALID", INT64_VALUE_ERROR);
  }
  const parsed = BigInt(rendered);
  if (parsed < INT64_MIN || parsed > INT64_MAX) {
    throw structuredError("OP_INVALID", `${INT64_RANGE_ERROR}; got ${rendered}`);
  }
  return rendered;
}
function int64(value) {
  return Object.freeze({
    [INT64_VALUE_BRAND]: true,
    int64: requireInt64String(value)
  });
}
function requireDecimalString(value) {
  if (typeof value !== "string" || !DECIMAL_STRING_RE.test(value)) {
    throw structuredError("OP_INVALID", DECIMAL_VALUE_ERROR);
  }
  return value;
}
function decimal(value) {
  return Object.freeze({
    [DECIMAL_VALUE_BRAND]: true,
    decimal: requireDecimalString(value)
  });
}
function requireBase64String(value) {
  if (typeof value !== "string" || !BASE64_STRING_RE.test(value)) {
    throw structuredError("OP_INVALID", BYTES_VALUE_ERROR);
  }
  const padded = value + "=".repeat((4 - value.length % 4) % 4);
  try {
    const normalized = btoa(atob(padded));
    if (normalized !== padded) throw new Error("non-canonical base64");
    return normalized;
  } catch {
    throw structuredError("OP_INVALID", BYTES_VALUE_ERROR);
  }
}
function byteValue(bytes) {
  return Object.freeze({
    [BYTES_VALUE_BRAND]: true,
    bytes: bytes instanceof Uint8Array ? bytesToBase64(bytes) : requireBase64String(bytes)
  });
}
function nativeDbExprNode(value) {
  if (value === nativeDateNow) return { node: "fnSynth", fn: "now", args: [] };
  if (value === nativeMathRandom) return { node: "uuidV4" };
  if (nativeCryptoRandomUUID !== void 0 && value === nativeCryptoRandomUUID) {
    return { node: "uuidV4" };
  }
  return void 0;
}
var INVALID_FUNCTION_VALUE_MESSAGE = "function values are not valid here; only the supported native symbols Date.now / Math.random / crypto.randomUUID translate to DB-evaluated scalars";
function rejectFunctionValue(value) {
  if (typeof value === "function") {
    throw structuredError("OP_INVALID", INVALID_FUNCTION_VALUE_MESSAGE);
  }
}
var active = null;
function structuredError(code, message, extra) {
  const err = new Error(message);
  err.code = code;
  if (extra) Object.assign(err, extra);
  return err;
}
function __begin() {
  active = {
    ops: [],
    pending: /* @__PURE__ */ new Map(),
    nextSelectorId: 0
  };
}
function __drain() {
  if (active === null) return [];
  const rec = active;
  active = null;
  for (const sel of rec.pending.values()) {
    if (!sel.terminated) {
      throw structuredError(
        "SELECTOR_NOT_TERMINATED",
        `selector .${sel.selector}(${JSON.stringify(sel.name)}) was never terminated; a selector records nothing until one of its terminals is called`,
        {
          selector: sel.selector,
          name: sel.name,
          suggested_fix: `call a terminal on .${sel.selector}(${JSON.stringify(sel.name)}) (e.g. .add({\u2026})) or remove the selector`
        }
      );
    }
  }
  return rec.ops;
}
function recorder() {
  if (active === null) {
    throw structuredError(
      "OP_OUTSIDE_RECORDER",
      "migration operations may only be authored synchronously inside up()",
      { suggested_fix: "move the operation call inside the migration's up() body" }
    );
  }
  return active;
}
function push(op) {
  recorder().ops.push(op);
  return op;
}
function __pgPush(op) {
  return push(op);
}
var tier1Producers = [];
function defineOp(kind, producer = kind) {
  tier1Producers.push({ kind, producer });
  return (payload) => push(compact({ op: kind, ...payload }));
}
function opProducers() {
  return tier1Producers;
}
function opProducerRegistry() {
  const byKind = /* @__PURE__ */ new Map();
  for (const producer of tier1Producers) {
    const list = byKind.get(producer.kind);
    if (list === void 0) byKind.set(producer.kind, [producer]);
    else list.push(producer);
  }
  return byKind;
}
var emitCreateEnum = defineOp("createEnum");
var emitDropEnum = defineOp("dropEnum");
var emitCreateDomain = defineOp("createDomain");
var emitDropDomain = defineOp("dropDomain");
var emitCreateSequence = defineOp("createSequence");
var emitAlterSequence = defineOp("alterSequence");
var emitDropSequence = defineOp("dropSequence");
var emitCreateSchema = defineOp("createSchema");
var emitDropSchema = defineOp("dropSchema");
var emitCreateExtension = defineOp("createExtension");
var emitDropExtension = defineOp("dropExtension");
var emitCreateRole = defineOp("createRole");
var emitAlterRole = defineOp("alterRole");
var emitDropRole = defineOp("dropRole");
var emitComment = defineOp("comment");
var emitCreateTable = defineOp("createTable");
var emitCreatePartition = defineOp("createPartition");
var emitAttachPartition = defineOp("attachPartition");
var emitDetachPartition = defineOp("detachPartition");
var emitDropPartition = defineOp("dropPartition");
var emitSetTableOptions = defineOp("setTableOptions");
var emitDropTable = defineOp("dropTable");
var emitRenameTable = defineOp("renameTable");
var emitAlterPrimaryKey = defineOp("alterPrimaryKey");
var emitSynchronizeIdentity = defineOp("synchronizeIdentity");
var emitAddColumn = defineOp("addColumn");
var emitAddColumnUnique = defineOp("addConstraint", "addColumn.unique");
var emitDropColumn = defineOp("dropColumn");
var emitRenameColumn = defineOp("renameColumn");
var emitSetColumnType = defineOp("setColumnType");
var emitSetColumnNotNull = defineOp("setColumnNotNull");
var emitDropColumnNotNull = defineOp("dropColumnNotNull");
var emitSetColumnDefault = defineOp("setColumnDefault");
var emitDropColumnDefault = defineOp("dropColumnDefault");
var emitAddForeignKey = defineOp("addConstraint", "foreignKey");
var emitAddUnique = defineOp("addConstraint", "unique");
var emitAddCheck = defineOp("addConstraint", "check");
var emitAddExclusion = defineOp("addConstraint", "exclusion");
var emitDropConstraint = defineOp("dropConstraint");
var emitValidateConstraint = defineOp("validateConstraint");
var emitCreateIndex = defineOp("createIndex");
var emitDropIndex = defineOp("dropIndex");
var emitInsert = defineOp("insert");
var emitUpdate = defineOp("update");
var emitDelete = defineOp("delete");
var emitBackfill = defineOp("backfill");
var emitDialectal = defineOp("dialectal");
var emitCreateView = defineOp("createView", "view.create");
var emitDropView = defineOp("dropView");
var emitSetRls = defineOp("setRls");
var emitCreatePolicy = defineOp("createPolicy");
var emitDropPolicy = defineOp("dropPolicy");
var emitCreateTrigger = defineOp("createTrigger");
var emitDropTrigger = defineOp("dropTrigger");
function registerSelector(selector, name) {
  const rec = recorder();
  const id = rec.nextSelectorId++;
  rec.pending.set(id, { selector, name, terminated: false });
  return id;
}
function terminateSelector(id) {
  const rec = recorder();
  const sel = rec.pending.get(id);
  if (sel === void 0) return;
  if (sel.terminated) {
    throw structuredError(
      "SELECTOR_ALREADY_TERMINATED",
      `selector .${sel.selector}(${JSON.stringify(sel.name)}) was terminated twice; each selector records exactly one op`,
      { selector: sel.selector, name: sel.name }
    );
  }
  sel.terminated = true;
}
function compact(obj) {
  for (const k of Object.keys(obj)) {
    if (obj[k] === void 0) delete obj[k];
  }
  return obj;
}
function setOwn(target, key, value) {
  Object.defineProperty(target, key, {
    value,
    enumerable: true,
    configurable: true,
    writable: true
  });
}
function requireString(v, what) {
  if (typeof v !== "string") {
    throw structuredError("OP_INVALID", `${what} must be a string; got ${typeof v}`);
  }
}
function requireNonEmptyString(v, what) {
  requireString(v, what);
  if (v.length === 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-empty string`);
  }
}
function requireOrderedColumns(v, what) {
  if (!Array.isArray(v) || v.length === 0) {
    throw structuredError(
      "OP_INVALID",
      `${what} must be a non-empty ordered column-name array`
    );
  }
  const seen = /* @__PURE__ */ new Set();
  for (let position = 0; position < v.length; position += 1) {
    const column = v[position];
    requireNonEmptyString(column, `${what}[${position}]`);
    if (seen.has(column)) {
      throw structuredError(
        "OP_INVALID",
        `${what} names column ${JSON.stringify(column)} more than once`,
        { column, position }
      );
    }
    seen.add(column);
  }
}
function requireDropIdentitySubset(dropIdentityFrom, expectedColumns, what) {
  if (dropIdentityFrom === void 0) return;
  const expected = new Set(expectedColumns);
  for (const column of dropIdentityFrom) {
    if (!expected.has(column)) {
      throw structuredError(
        "OP_INVALID",
        `${what} names column ${JSON.stringify(column)}, which is not in expectedColumns`,
        { column }
      );
    }
  }
}
function requireStrictness(v, what) {
  if (v === void 0) return void 0;
  if (v !== "strict" && v !== "lenient" && v !== "off") {
    throw structuredError("OP_INVALID", `${what} must be "strict", "lenient", or "off"`);
  }
  return v;
}
function requireOptionalBoolean(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "boolean") {
    throw structuredError("OP_INVALID", `${what} must be a boolean`);
  }
  return v;
}
function requirePlainObject(v, what) {
  if (v === null || typeof v !== "object" || Array.isArray(v)) {
    throw structuredError("OP_INVALID", `${what} must be an object`);
  }
}
function requireTypeIdPrefix(v, what = "ids.typeId({ prefix })") {
  requireString(v, what);
  if (v.length > 63 || v !== "" && !/^[a-z](?:[a-z_]*[a-z])?$/.test(v)) {
    throw structuredError(
      "OP_INVALID",
      `${what}: prefix must be empty or at most 63 lowercase ASCII letters/underscores beginning and ending with a letter`,
      { prefix: v }
    );
  }
}
function requireOptionalPositiveInteger(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "number" || !Number.isInteger(v) || v <= 0) {
    throw structuredError("OP_INVALID", `${what} must be a positive integer; got ${v}`);
  }
  return v;
}
function requireOptionalNonNegativeInteger(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "number" || !Number.isInteger(v) || v < 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-negative integer; got ${v}`);
  }
  return v;
}
function indexElementFacet(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "string" || v.length === 0) {
    throw structuredError("OP_INVALID", `${what} must be a non-empty string`);
  }
  return v;
}
function runtimeOptionsFromCreateArgs(args) {
  const opts = args.options;
  if (opts === void 0) return void 0;
  requirePlainObject(opts, "create({ options })");
  const softDelete = requireOptionalBoolean(opts.softDelete, "create({ options: { softDelete } })");
  const versioning = requireOptionalBoolean(opts.versioning, "create({ options: { versioning } })");
  const strictness = requireStrictness(opts.strictness, "create({ options: { strictness } })");
  const hasOptions = softDelete !== void 0 || versioning !== void 0 || strictness !== void 0;
  if (!hasOptions) return void 0;
  return compact({
    softDelete: softDelete ?? false,
    versioning: versioning ?? false,
    strictness: strictness ?? "strict"
  });
}
function runtimeOptionsPatchFromArgs(args) {
  requirePlainObject(args, "setOptions(args)");
  const softDelete = requireOptionalBoolean(args.softDelete, "setOptions({ softDelete })");
  const versioning = requireOptionalBoolean(args.versioning, "setOptions({ versioning })");
  const strictness = requireStrictness(args.strictness, "setOptions({ strictness })");
  const patch = compact({
    softDelete,
    versioning,
    strictness
  });
  if (Object.keys(patch).length === 0) {
    throw structuredError(
      "OP_INVALID",
      "setOptions(...) must set at least one of softDelete, versioning, or strictness"
    );
  }
  return patch;
}
function requireSafeI64(v, what) {
  if (v === void 0) return void 0;
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
  if (v === void 0) return void 0;
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
var VECTOR_METRICS = ["cosine", "l2", "innerProduct"];
var SEQUENCE_AS_TYPES = ["int", "bigInt"];
var MASK_KINDS = [
  "full",
  "last4",
  "first4",
  "email",
  "name",
  "date-year",
  "date-decade",
  "none"
];
var MASK_CLASSIFICATIONS = [
  "public",
  "pii",
  "spi",
  "phi",
  "pci",
  "internal"
];
var REF_ACTIONS = [
  "cascade",
  "restrict",
  "setNull",
  "setDefault",
  "noAction"
];
function requireReferenceAction(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "string" || !REF_ACTIONS.includes(v)) {
    throw structuredError(
      "OP_INVALID",
      `${what} must be one of ${REF_ACTIONS.join(" | ")}; got ${JSON.stringify(v)}`
    );
  }
  return v;
}
var ColumnDefImpl = class _ColumnDefImpl {
  _type;
  _nullable;
  _default;
  _primaryKey;
  _unique;
  _reference;
  // Semantic facets carried on the IrColumn: canonical value format
  // (`ids.typeId({prefix})`), pgvector distance metric, and the remaining
  // standalone column facets.
  // Absent ⇒ omitted on the wire.
  _valueFormat;
  _vectorMetric;
  _caseSensitive;
  _mask;
  _generated;
  _identity;
  constructor(colType, fields) {
    this._type = colType;
    this._nullable = fields?.nullable ?? true;
    this._default = fields?.default;
    this._primaryKey = fields?.primaryKey ?? false;
    this._unique = fields?.unique ?? false;
    this._reference = fields?.reference;
    this._valueFormat = fields?.valueFormat;
    this._vectorMetric = fields?.vectorMetric;
    this._caseSensitive = fields?.caseSensitive;
    this._mask = fields?.mask;
    this._generated = fields?.generated;
    this._identity = fields?.identity;
  }
  /** Clone with the named fields overridden — the basis of immutability. */
  with(over) {
    return new _ColumnDefImpl(over.type ?? this._type, {
      nullable: over.nullable ?? this._nullable,
      default: "default" in over ? over.default : this._default,
      primaryKey: over.primaryKey ?? this._primaryKey,
      unique: over.unique ?? this._unique,
      reference: "reference" in over ? over.reference : this._reference,
      valueFormat: "valueFormat" in over ? over.valueFormat : this._valueFormat,
      vectorMetric: "vectorMetric" in over ? over.vectorMetric : this._vectorMetric,
      caseSensitive: "caseSensitive" in over ? over.caseSensitive : this._caseSensitive,
      mask: "mask" in over ? over.mask : this._mask,
      generated: "generated" in over ? over.generated : this._generated,
      identity: "identity" in over ? over.identity : this._identity
    });
  }
  /** Internal: carry the pgvector distance metric (`t.vector({ dimensions, metric })`). */
  __withVectorMetric(metric) {
    return this.with({ vectorMetric: metric });
  }
  notNull() {
    return this.with({ nullable: false });
  }
  default(value) {
    return this.with({ default: toIrDefault(value) });
  }
  primaryKey() {
    return this.with({ primaryKey: true, nullable: false });
  }
  unique() {
    return this.with({ unique: true });
  }
  references(table2, column, options = {}) {
    requireString(table2, "t.*.references(table, column, options): table");
    if (table2.length === 0) {
      throw structuredError(
        "OP_INVALID",
        "t.*.references(table, column, options): table must be a non-empty string"
      );
    }
    requireString(column, "t.*.references(table, column, options): column");
    if (column.length === 0) {
      throw structuredError(
        "OP_INVALID",
        "t.*.references(table, column, options): column must be a non-empty string"
      );
    }
    requirePlainObject(options, "t.*.references(table, column, options): options");
    if (options.name !== void 0) {
      requireNonEmptyString(options.name, "t.*.references(table, column, { name })");
    }
    const reference = compact({
      table: table2,
      column,
      name: options.name,
      onDelete: requireReferenceAction(
        options.onDelete,
        "t.*.references(table, column, { onDelete })"
      ),
      onUpdate: requireReferenceAction(
        options.onUpdate,
        "t.*.references(table, column, { onUpdate })"
      )
    });
    return this.with({ reference: Object.freeze(reference) });
  }
  /** `.mask({ kind, classification? })` — declare a STANDALONE column mask so
   *  the field reads back as `MaskedValue<T>` and the op lower emits the `zero-migrate:mask`
   *  sentinel + `_masked` sibling. `kind` is REQUIRED (closed `MASK_KINDS`);
   *  `classification` is optional and DEFAULTS to `"pii"` (closed
   *  `MASK_CLASSIFICATIONS`). The closed-set checks mirror `t.vector({ dimensions, metric })`:
   *  a friendly client-side OP_INVALID over the SAME closed set the engine's enums
   *  enforce authoritatively. */
  mask(opts) {
    if (opts === null || typeof opts !== "object") {
      throw structuredError("OP_INVALID", "t.*.mask(opts): opts must be { kind, classification? }");
    }
    requireString(opts.kind, "t.*.mask({ kind })");
    if (!MASK_KINDS.includes(opts.kind)) {
      throw structuredError(
        "OP_INVALID",
        `t.*.mask({ kind }): kind must be one of ${MASK_KINDS.join(" | ")}, got ${JSON.stringify(opts.kind)}`,
        { kind: opts.kind }
      );
    }
    const classification = opts.classification === void 0 ? "pii" : opts.classification;
    if (!MASK_CLASSIFICATIONS.includes(classification)) {
      throw structuredError(
        "OP_INVALID",
        `t.*.mask({ classification }): classification must be one of ${MASK_CLASSIFICATIONS.join(" | ")}, got ${JSON.stringify(classification)}`,
        { classification }
      );
    }
    return this.with({ mask: { kind: opts.kind, classification } });
  }
  generated(expr, opts) {
    if (opts !== void 0 && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.generated(expr, opts): opts must be { virtual?: boolean }");
    }
    if (opts?.virtual !== void 0 && typeof opts.virtual !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.generated(expr, { virtual }): virtual must be a boolean");
    }
    return this.with({
      generated: {
        expr: resolveImmutableExpr(expr, "generated column expression"),
        stored: opts?.virtual === true ? false : true
      }
    });
  }
  identity(opts) {
    if (opts !== void 0 && (opts === null || typeof opts !== "object")) {
      throw structuredError("OP_INVALID", "t.*.identity(opts): opts must be { always?: boolean }");
    }
    if (opts?.always !== void 0 && typeof opts.always !== "boolean") {
      throw structuredError("OP_INVALID", "t.*.identity({ always }): always must be a boolean");
    }
    return this.with({ identity: { always: opts?.always === true } });
  }
  autoIncrement() {
    return this.with({ identity: { always: false } });
  }
  __toIrColumn(name) {
    return compact({
      name,
      type: this._type,
      nullable: this._nullable === false ? false : void 0,
      default: this._default,
      // A PRIMARY KEY already IMPLIES uniqueness, so a column that is BOTH
      // `.unique()` and `.primaryKey()` would otherwise carry a redundant
      // column-level UNIQUE (an extra index/constraint) on top of the table's pk
      // constraint. Suppress it (lock-step with the addColumn path + the differ,
      // which never emits a separate UNIQUE for the PK column).
      unique: this._unique && !this._primaryKey ? true : void 0,
      // Carry the semantic facets onto the wire IrColumn (camelCase keys
      // `valueFormat`/`references`/`vectorMetric`/`mask`). Absent ⇒ omitted, so a
      // plain column is byte-identical to the pre-facet image (checksum-neutral).
      valueFormat: this._valueFormat,
      references: this._reference,
      vectorMetric: this._vectorMetric,
      caseSensitive: this._caseSensitive === false ? false : void 0,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity
    });
  }
  __toAddColumnTail() {
    rejectColumnReferenceFacet(
      this,
      ".column(name).add({ type }): typed references are not a lifecycle operation"
    );
    return compact({
      type: this._type,
      nullable: this._nullable === false ? false : void 0,
      default: this._default,
      // Carry the value format + remaining column facets onto the addColumn op tail
      // (camelCase keys, lock-step with `Op::AddColumn`). Absent ⇒ omitted (compact).
      valueFormat: this._valueFormat,
      vectorMetric: this._vectorMetric,
      caseSensitive: this._caseSensitive === false ? false : void 0,
      mask: this._mask,
      generated: this._generated,
      identity: this._identity
    });
  }
};
function isColumnDef(x) {
  return x instanceof ColumnDefImpl;
}
function rejectColumnReferenceFacet(def, where) {
  if (def._reference !== void 0) {
    throw structuredError(
      "OP_INVALID",
      `${where} cannot use a .references() ColumnDef; typed references are supported only in table(...).create({ columns })`
    );
  }
}
function textColumn(opts) {
  if (opts !== void 0 && (opts === null || typeof opts !== "object")) {
    throw structuredError("OP_INVALID", "t.text(opts): opts must be { caseSensitive?: boolean }");
  }
  if (opts?.caseSensitive !== void 0 && typeof opts.caseSensitive !== "boolean") {
    throw structuredError("OP_INVALID", "t.text({ caseSensitive }): caseSensitive must be a boolean");
  }
  return new ColumnDefImpl("text", {
    caseSensitive: opts?.caseSensitive === false ? false : void 0
  });
}
function stringColumn(opts) {
  if (opts !== void 0 && (opts === null || typeof opts !== "object")) {
    throw structuredError(
      "OP_INVALID",
      "t.string(opts): opts must be { length?: number, caseSensitive?: boolean }"
    );
  }
  const length = requireOptionalPositiveInteger(opts?.length, "t.string({ length })") ?? 255;
  if (opts?.caseSensitive !== void 0 && typeof opts.caseSensitive !== "boolean") {
    throw structuredError("OP_INVALID", "t.string({ caseSensitive }): caseSensitive must be a boolean");
  }
  return new ColumnDefImpl({ string: { length } }, {
    caseSensitive: opts?.caseSensitive === false ? false : void 0
  });
}
function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}
function isPlainObject(value) {
  if (value === null || typeof value !== "object") return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}
function isInt64Value(value) {
  return value !== null && typeof value === "object" && value[INT64_VALUE_BRAND] === true && typeof value.int64 === "string";
}
function isDecimalValue(value) {
  return value !== null && typeof value === "object" && value[DECIMAL_VALUE_BRAND] === true && typeof value.decimal === "string";
}
function isBytesValue(value) {
  return value !== null && typeof value === "object" && value[BYTES_VALUE_BRAND] === true && typeof value.bytes === "string";
}
function isPerRowGenerator(generator) {
  if (generator === "uuidV4" || generator === "uuidV7" || generator === "ulid") {
    return true;
  }
  if (!isPlainObject(generator)) return false;
  const keys = Object.keys(generator);
  if (keys.length !== 1 || keys[0] !== "typeId" || !isPlainObject(generator.typeId)) {
    return false;
  }
  const typeIdKeys = Object.keys(generator.typeId);
  return typeIdKeys.length === 1 && typeIdKeys[0] === "prefix" && typeof generator.typeId.prefix === "string";
}
function perRowGeneratorOf(value) {
  if (value === null || typeof value !== "object") return void 0;
  const generator = value[PER_ROW_GENERATOR_BRAND];
  return isPerRowGenerator(generator) ? generator : void 0;
}
function perRowGeneratorValue(generator) {
  return Object.freeze({
    [PER_ROW_GENERATOR_BRAND]: generator
  });
}
function rejectPerRowGeneratorValue(value) {
  if (perRowGeneratorOf(value) !== void 0) {
    throw structuredError(
      "OP_INVALID",
      "perRow.* values are valid only inside backfill({ set }); they are not scalar values, SQL expressions, or column defaults"
    );
  }
}
function rejectNestedPerRowGeneratorValues(value, seen = /* @__PURE__ */ new WeakSet()) {
  rejectPerRowGeneratorValue(value);
  if (value === null || typeof value !== "object") return;
  if (seen.has(value)) return;
  seen.add(value);
  for (const key of Reflect.ownKeys(value)) {
    const descriptor = Object.getOwnPropertyDescriptor(value, key);
    if (descriptor !== void 0 && "value" in descriptor) {
      rejectNestedPerRowGeneratorValues(descriptor.value, seen);
    }
  }
}
function isRemovedDecimalCarrier(value) {
  if (!isPlainObject(value) || isDecimalValue(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "decimal" && typeof value.decimal === "string";
}
function isRemovedInt64Carrier(value) {
  if (!isPlainObject(value) || isInt64Value(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "int64" && typeof value.int64 === "string";
}
function isRemovedBytesCarrier(value) {
  if (!isPlainObject(value) || isBytesValue(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 1 && keys[0] === "bytes" && typeof value.bytes === "string";
}
function rejectNestedFunctionValues(value) {
  rejectPerRowGeneratorValue(value);
  rejectFunctionValue(value);
  if (Array.isArray(value)) {
    for (const item of value) rejectNestedFunctionValues(item);
  } else if (isPlainObject(value)) {
    for (const item of Object.values(value)) rejectNestedFunctionValues(item);
  }
}
function finiteNumberDecimalString(value) {
  const rendered = String(value);
  if (!/[eE]/.test(rendered)) return rendered;
  const match = /^(-?)(\d+)(?:\.(\d+))?[eE]([+-]?\d+)$/.exec(rendered);
  if (match === null) {
    throw structuredError(
      "OP_INVALID",
      'number scalar could not be represented exactly; use decimal("<n>")'
    );
  }
  const [, sign, whole, fraction = "", exponentText] = match;
  const digits = whole + fraction;
  const decimalAt = whole.length + Number(exponentText);
  let expanded;
  if (decimalAt <= 0) {
    expanded = `${sign}0.${"0".repeat(-decimalAt)}${digits}`;
  } else if (decimalAt >= digits.length) {
    expanded = `${sign}${digits}${"0".repeat(decimalAt - digits.length)}`;
  } else {
    expanded = `${sign}${digits.slice(0, decimalAt)}.${digits.slice(decimalAt)}`;
  }
  return requireDecimalString(expanded);
}
function toIrScalar(value) {
  rejectNestedFunctionValues(value);
  if (isInt64Value(value)) return { int64: requireInt64String(value.int64) };
  if (isDecimalValue(value)) return { decimal: requireDecimalString(value.decimal) };
  if (isBytesValue(value)) return { bytes: requireBase64String(value.bytes) };
  if (typeof value === "bigint") {
    throw structuredError("OP_INVALID", "raw bigint is not a migration scalar \u2014 use int64(...) instead");
  }
  if (isRemovedInt64Carrier(value)) {
    throw structuredError("OP_INVALID", "the { int64 } carrier is not an authored value \u2014 use int64(...)");
  }
  if (isRemovedDecimalCarrier(value)) {
    throw structuredError("OP_INVALID", 'the { decimal } carrier is removed \u2014 use decimal("<n>")');
  }
  if (isRemovedBytesCarrier(value)) {
    throw structuredError("OP_INVALID", "the { bytes } carrier is removed \u2014 use byteValue(...)");
  }
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw structuredError("OP_INVALID", "number scalar must be finite");
    }
    if (Number.isInteger(value)) {
      if (!Number.isSafeInteger(value)) {
        throw structuredError(
          "OP_INVALID",
          "integer scalar must be a JS safe integer; use int64(...) for exact signed 64-bit values"
        );
      }
    } else {
      return { decimal: finiteNumberDecimalString(value) };
    }
  }
  if (value instanceof Uint8Array) return { bytes: bytesToBase64(value) };
  if (value === null || typeof value === "string" || typeof value === "boolean" || typeof value === "number") {
    return value;
  }
  if (value === void 0) {
    throw structuredError(
      "OP_INVALID",
      "scalar value cannot be undefined; use null explicitly"
    );
  }
  if (typeof value === "symbol") {
    throw structuredError("OP_INVALID", "symbol is not a supported scalar value");
  }
  throw structuredError(
    "OP_INVALID",
    "scalar value must be null, a string, boolean, finite number, int64(...), decimal(...), byteValue(...), or Uint8Array"
  );
}
function toIrValue(value) {
  rejectNestedPerRowGeneratorValues(value);
  const synth = nativeDbExprNode(value);
  if (synth !== void 0) return synth;
  rejectFunctionValue(value);
  if (value instanceof ExprChainImpl) return value.__node;
  if (value && typeof value === "object" && typeof value.node === "string") return value;
  return toIrScalar(value);
}
var JSON_DEFAULT_INTEGER_ERROR = "json default values support integers only (floats not yet supported)";
var JSON_DEFAULT_VALUE_ERROR = "json default values must be JSON values (null, boolean, integer, string, array, or object)";
function toIrJsonValue(value) {
  rejectPerRowGeneratorValue(value);
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
    const out = {};
    for (const key of Object.keys(value).sort()) {
      setOwn(out, key, toIrJsonValue(value[key]));
    }
    return out;
  }
  throw structuredError("OP_INVALID", JSON_DEFAULT_VALUE_ERROR);
}
var DEFAULT_SCALAR_FNS = /* @__PURE__ */ new Set([
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
  "replace"
]);
var DEFAULT_SYNTH_FNS = /* @__PURE__ */ new Set([
  "now",
  "concatWs",
  "splitPart"
]);
var IMMUTABLE_SCALAR_FNS = /* @__PURE__ */ new Set([
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
  "replace"
]);
var IMMUTABLE_SYNTH_FNS = /* @__PURE__ */ new Set([
  "concatWs",
  "splitPart"
]);
var IMMUTABLE_HELPERS = "lower/upper/trim/length/abs/coalesce/nullif/mod/round/floor/ceil/substr/replace/extract/concatWs/splitPart";
function defaultBuilder() {
  return Object.freeze({ case: caseExpr });
}
function defaultFunctionValueError() {
  return structuredError(
    "OP_INVALID",
    "function defaults must be authored with top-level value constructors, e.g. `.default(now())` or `.default(uuidV4())`; the old `{ fn: ... }` and bare native-symbol default forms are removed"
  );
}
function rejectRemovedDefaultFunctionValue(value) {
  if (value === nativeDateNow || value === nativeMathRandom || nativeCryptoRandomUUID !== void 0 && value === nativeCryptoRandomUUID) {
    throw defaultFunctionValueError();
  }
}
function isExprNode(value) {
  return Boolean(value && typeof value === "object" && typeof value.node === "string");
}
function resolveDefaultExpr(slot) {
  rejectRemovedDefaultFunctionValue(slot);
  const resolved = typeof slot === "function" ? slot(defaultBuilder()) : slot;
  if (resolved instanceof ExprChainImpl) return resolved.__node;
  if (isExprNode(resolved)) return resolved;
  return exprArg(resolved);
}
function validateDefaultExpr(expr) {
  rejectNestedPerRowGeneratorValues(expr);
  const walk = (node) => {
    if (!node || typeof node !== "object" || typeof node.node !== "string") {
      throw structuredError("OP_INVALID", "default expression must be a closed Expr node");
    }
    const n = node;
    switch (n.node) {
      case "colRef":
        throw structuredError("OP_INVALID", "a column default cannot reference a column");
      case "agg":
        if (n.arg !== void 0 && n.arg !== null) walk(n.arg);
        if (n.delimiter !== void 0 && n.delimiter !== null) walk(n.delimiter);
        return;
      case "literal":
        return;
      case "uuidV4":
      case "uuidV7":
        return;
      case "fnCall": {
        if (typeof n.fn !== "string" || !DEFAULT_SCALAR_FNS.has(n.fn)) {
          throw structuredError(
            "OP_INVALID",
            "a column default cannot use volatile or vendor-only functions; use immutable scalar chain helpers"
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
        if (n.else !== void 0 && n.else !== null) walk(n.else);
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
          "a column default cannot use volatile, dialect-specific, or vendor-only expression nodes"
        );
      case "extract":
        throw structuredError("OP_INVALID", "a column default cannot use an EXTRACT expression");
      default:
        throw structuredError("OP_INVALID", `unsupported default expression node ${JSON.stringify(n.node)}`);
    }
  };
  walk(expr);
}
function defaultExprIr(slot) {
  const expr = resolveDefaultExpr(slot);
  validateDefaultExpr(expr);
  return { expr };
}
function toIrDefault(value) {
  rejectPerRowGeneratorValue(value);
  if (typeof value === "function" || value instanceof ExprChainImpl || isExprNode(value)) {
    return defaultExprIr(value);
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
  if (isInt64Value(value)) return { literal: { value: toIrScalar(value) } };
  if (isDecimalValue(value)) return { literal: { value: toIrScalar(value) } };
  if (isBytesValue(value)) return { literal: { value: toIrScalar(value) } };
  if (isPlainObject(value)) {
    if (Object.keys(value).length === 0) return { container: "object" };
    if (isRemovedInt64Carrier(value)) return { literal: { value: toIrScalar(value) } };
    if (isRemovedDecimalCarrier(value)) return { literal: { value: toIrScalar(value) } };
    if (isRemovedBytesCarrier(value)) return { literal: { value: toIrScalar(value) } };
    rejectNestedFunctionValues(value);
    return { json: toIrJsonValue(value) };
  }
  return { literal: { value: toIrScalar(value) } };
}
function isNextvalDefault(value) {
  return value !== null && typeof value === "object" && value[NEXTVAL_DEFAULT_MARKER] === true;
}
function nextval(name, opts = {}) {
  requireString(name, "nextval(name)");
  if (opts === null || typeof opts !== "object" || Array.isArray(opts)) {
    throw structuredError("OP_INVALID", "nextval(name, opts): opts must be { schema?: string }");
  }
  if (opts.schema !== void 0) requireString(opts.schema, "nextval(name, { schema })");
  return Object.freeze({
    [NEXTVAL_DEFAULT_MARKER]: true,
    name,
    schema: opts.schema
  });
}
function now() {
  return chain({ node: "fnSynth", fn: "now", args: [] });
}
function uuidV4() {
  return chain({ node: "uuidV4" });
}
function uuidV7() {
  return chain({ node: "uuidV7" });
}
function genRandomUuid() {
  return uuidV4();
}
function currentSetting(name, opts = {}) {
  requireString(name, "currentSetting(name)");
  if (opts === null || typeof opts !== "object" || Array.isArray(opts)) {
    throw structuredError("OP_INVALID", "currentSetting(name, opts): opts must be { missingOk?: boolean }");
  }
  const missingOk = requireOptionalBoolean(opts.missingOk, "currentSetting(name, { missingOk })");
  return chain({
    node: "fnCall",
    fn: "currentSetting",
    args: missingOk === void 0 ? [{ node: "literal", value: name }] : [{ node: "literal", value: name }, { node: "literal", value: missingOk }]
  });
}
function currentUser() {
  return chain({ node: "fnCall", fn: "currentUser", args: [] });
}
function interval(duration) {
  return chain({
    node: "pgInterval",
    duration: pgDuration(duration)
  });
}
var ids = {
  typeId: (opts) => {
    requirePlainObject(opts, "ids.typeId(opts)");
    requireTypeIdPrefix(opts.prefix);
    return new ColumnDefImpl("text", {
      valueFormat: { typeId: { prefix: opts.prefix } }
    });
  },
  ulid: () => new ColumnDefImpl("text", { valueFormat: "ulid" })
};
var perRow = Object.freeze({
  uuidV4: () => perRowGeneratorValue("uuidV4"),
  uuidV7: () => perRowGeneratorValue("uuidV7"),
  typeId: (opts) => {
    requirePlainObject(opts, "perRow.typeId(opts)");
    requireTypeIdPrefix(opts.prefix, "perRow.typeId({ prefix })");
    const generator = Object.freeze({
      typeId: Object.freeze({ prefix: opts.prefix })
    });
    return perRowGeneratorValue(generator);
  },
  ulid: () => perRowGeneratorValue("ulid")
});
var t = {
  text: (opts) => textColumn(opts),
  string: (opts) => stringColumn(opts),
  textArray: () => new ColumnDefImpl("textArray"),
  numeric: (opts = {}) => {
    requirePlainObject(opts, "t.numeric(opts)");
    const precision = requireOptionalPositiveInteger(opts.precision, "t.numeric({ precision })") ?? 38;
    const scale = requireOptionalNonNegativeInteger(opts.scale, "t.numeric({ scale })") ?? 9;
    return new ColumnDefImpl({ decimal: { precision, scale } });
  },
  char: (opts) => {
    requirePlainObject(opts, "t.char(opts)");
    const n = requireOptionalPositiveInteger(opts.length, "t.char({ length })");
    if (n === void 0) {
      throw structuredError("OP_INVALID", "t.char({ length }) requires length");
    }
    return new ColumnDefImpl({ char: { length: n } });
  },
  timestamp: () => new ColumnDefImpl("timestamp"),
  date: () => new ColumnDefImpl("date"),
  uuid: () => new ColumnDefImpl("uuid"),
  bytes: () => new ColumnDefImpl("bytes"),
  boolean: () => new ColumnDefImpl("boolean"),
  json: () => new ColumnDefImpl("json"),
  vector: (opts) => {
    requirePlainObject(opts, "t.vector(opts)");
    const n = requireOptionalPositiveInteger(opts.dimensions, "t.vector({ dimensions })");
    if (n === void 0) {
      throw structuredError("OP_INVALID", "t.vector({ dimensions }) requires dimensions");
    }
    let col = new ColumnDefImpl({ vector: { vector: n } });
    if (opts.metric !== void 0) {
      requireString(opts.metric, "t.vector({ metric })");
      if (!VECTOR_METRICS.includes(opts.metric)) {
        throw structuredError(
          "OP_INVALID",
          `t.vector({ dimensions, metric }): metric must be one of ${VECTOR_METRICS.join(" | ")}, got ${JSON.stringify(opts.metric)}`,
          { metric: opts.metric }
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
    return new ColumnDefImpl({ enum: { name: n } });
  },
  domain: (name) => {
    const n = typeof name === "string" ? name : name.name;
    requireString(n, "t.domain(name)");
    return new ColumnDefImpl({ domain: { name: n } });
  },
  encrypted: (arg) => {
    const inner = arg && typeof arg === "object" && "of" in arg ? arg.of : arg;
    if (isColumnDef(inner)) {
      rejectColumnReferenceFacet(inner, "t.encrypted({ of })");
    }
    const innerType = isColumnDef(inner) ? inner._type : inner;
    if (innerType === void 0) {
      throw structuredError("OP_INVALID", "t.encrypted({ of }): of must be a ColumnDef or ColType");
    }
    return new ColumnDefImpl({ encrypted: { of: innerType } });
  }
};
function colTypeOf(typeArg) {
  if (isColumnDef(typeArg)) {
    rejectColumnReferenceFacet(typeArg, "this lifecycle or nested type position");
    return typeArg._type;
  }
  return typeArg;
}
function enumType(name) {
  requireString(name, "enumType(name)");
  const handle = {
    name,
    create(createArgs) {
      recordCreateEnum(name, createArgs);
      return handle;
    },
    drop(dropArgs = {}) {
      recordDropEnum(name, dropArgs);
      return handle;
    },
    comment(text, commentArgs = {}) {
      recordComment({ kind: "type", name, schema: commentArgs.schema }, text);
      return handle;
    }
  };
  return handle;
}
function __pgDomain(name) {
  requireString(name, "domain(name)");
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
    }
  };
  return handle;
}
function __pgSchema(name) {
  requireString(name, "schema(name)");
  let handle;
  const dropped = {
    name,
    create(args = {}) {
      recordCreateSchema(name, args);
      return handle;
    }
  };
  handle = {
    name,
    create(args = {}) {
      recordCreateSchema(name, args);
      return handle;
    },
    drop(args = {}) {
      recordDropSchema(name, args);
      return dropped;
    }
  };
  return handle;
}
function __pgExtension(name) {
  requireString(name, "extension(name)");
  let handle;
  const dropped = {
    name,
    create(args = {}) {
      recordCreateExtension(name, args);
      return handle;
    }
  };
  handle = {
    name,
    create(args = {}) {
      recordCreateExtension(name, args);
      return handle;
    },
    drop(args = {}) {
      recordDropExtension(name, args);
      return dropped;
    }
  };
  return handle;
}
function __pgRole(name) {
  requireString(name, "role(name)");
  let handle;
  const dropped = {
    name,
    create(args = {}) {
      recordCreateRole(name, args);
      return handle;
    }
  };
  handle = {
    name,
    create(args = {}) {
      recordCreateRole(name, args);
      return handle;
    },
    setOptions(args) {
      recordSetRoleOptions(name, args);
      return handle;
    },
    drop(args = {}) {
      recordDropRole(name, args);
      return dropped;
    }
  };
  return handle;
}
function __pgSequence(name) {
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
    }
  };
  return handle;
}
var domain = __pgDomain;
var schema = __pgSchema;
var extension = __pgExtension;
var role = __pgRole;
var sequence = __pgSequence;
function recordVendor(op) {
  return __pgPush(compact(op));
}
function dropOwnedBy(args) {
  if (!Array.isArray(args.roles)) {
    throw structuredError("OP_INVALID", "dropOwnedBy({ roles }): roles must be an array");
  }
  return recordVendor({ op: "dropOwnedBy", roles: args.roles });
}
function grant(args) {
  if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
    throw structuredError("OP_INVALID", "grant({ privileges }): privileges must be a non-empty array");
  }
  if (args.on === null || typeof args.on !== "object") {
    throw structuredError("OP_INVALID", "grant({ on }): on must be a target object");
  }
  if (!Array.isArray(args.to) || args.to.length === 0) {
    throw structuredError("OP_INVALID", "grant({ to }): to must be a non-empty array");
  }
  return recordVendor({
    op: "grant",
    privileges: args.privileges,
    on: args.on,
    to: args.to,
    withGrantOption: args.withGrantOption
  });
}
function revoke(args) {
  if (!Array.isArray(args.privileges) || args.privileges.length === 0) {
    throw structuredError("OP_INVALID", "revoke({ privileges }): privileges must be a non-empty array");
  }
  if (args.on === null || typeof args.on !== "object") {
    throw structuredError("OP_INVALID", "revoke({ on }): on must be a target object");
  }
  if (!Array.isArray(args.from) || args.from.length === 0) {
    throw structuredError("OP_INVALID", "revoke({ from }): from must be a non-empty array");
  }
  return recordVendor({
    op: "revoke",
    privileges: args.privileges,
    on: args.on,
    from: args.from
  });
}
function createFunction(args) {
  requireString(args.name, "createFunction({ name })");
  requireString(args.returns, "createFunction({ returns })");
  requireString(args.language, "createFunction({ language })");
  requireString(args.body, "createFunction({ body })");
  return recordVendor({
    op: "createFunction",
    name: args.name,
    schema: args.schema,
    args: args.args,
    returns: args.returns,
    language: args.language,
    replace: args.replace,
    volatility: args.volatility,
    body: args.body
  });
}
function dropFunction(args) {
  requireString(args.name, "dropFunction({ name })");
  return recordVendor({
    op: "dropFunction",
    name: args.name,
    schema: args.schema,
    argTypes: args.argTypes,
    ifExists: args.ifExists
  });
}
function raw(args) {
  requireString(args.sql, "raw({ sql })");
  requireString(args.reason, "raw({ reason })");
  return recordVendor({
    op: "pgRaw",
    sql: args.sql,
    reason: args.reason
  });
}
function chain(node) {
  return new ExprChainImpl(node);
}
function exprArg(x) {
  rejectNestedPerRowGeneratorValues(x);
  const synth = nativeDbExprNode(x);
  if (synth !== void 0) return synth;
  rejectFunctionValue(x);
  if (x instanceof ExprChainImpl) return x.__node;
  if (x && typeof x === "object" && typeof x.node === "string") return x;
  return { node: "literal", value: toIrScalar(x) };
}
function inListScalarKind(value, label) {
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
        `${label} must be a Scalar (string, number, boolean, or null); got ${typeof value}`
      );
  }
}
function scalarLiteralArray(values, what) {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a Scalar[]`);
  }
  let kind;
  return values.map((v, i) => {
    const elemKind = inListScalarKind(v, `${what}[${i}]`);
    if (kind === void 0) {
      kind = elemKind;
    } else if (elemKind !== kind) {
      throw structuredError("OP_INVALID", `${what} list must be homogeneous; ${what}[${i}] is ${elemKind}, expected ${kind}`);
    }
    return toIrScalar(v);
  });
}
function pgRegexPattern(pattern) {
  if (typeof pattern !== "string") {
    throw structuredError("OP_INVALID", `.regex(pattern): pattern must be a string; got ${typeof pattern}`);
  }
  if (pattern.length === 0) {
    throw structuredError("OP_INVALID", ".regex(pattern): pattern must be non-empty");
  }
  if (pattern.includes("\0")) {
    throw structuredError("OP_INVALID", ".regex(pattern): pattern must not contain a NUL byte");
  }
  return pattern;
}
var portableExtractFields = ["year", "month", "day", "hour", "minute", "dow"];
var portableExtractFieldSet = new Set(portableExtractFields);
var pgExtractFields = [
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
  "timezone_minute"
];
var pgExtractFieldSet = new Set(pgExtractFields);
var castTargets = ["text", "int", "real", "boolean", "bytes", "uuid"];
var castTargetSet = new Set(castTargets);
function castTarget(args) {
  if (!isPlainObject(args)) {
    throw structuredError("OP_INVALID", "cast(args): args must be { to }");
  }
  const to = args.to;
  if (typeof to !== "string" || !castTargetSet.has(to)) {
    throw structuredError(
      "OP_INVALID",
      `cast({ to }): to must be one of ${castTargets.map((t2) => JSON.stringify(t2)).join(", ")}; got ${JSON.stringify(to)}`
    );
  }
  return to;
}
function pgExtractField(field) {
  if (typeof field === "string" && portableExtractFieldSet.has(field)) {
    return field;
  }
  if (typeof field === "string" && pgExtractFieldSet.has(field)) {
    return field;
  }
  throw structuredError(
    "OP_INVALID",
    `.extract(field): field must be one of ${[...portableExtractFields, ...pgExtractFields].map((f) => JSON.stringify(f)).join(", ")}; got ${JSON.stringify(field)}`
  );
}
var durationFields = ["years", "months", "days", "hours", "minutes", "seconds"];
function pgDuration(duration) {
  if (!isPlainObject(duration)) {
    throw structuredError("OP_INVALID", `interval(duration): duration must be an object; got ${typeof duration}`);
  }
  const normalized = {};
  for (const key of durationFields) {
    const value = duration[key];
    if (value === void 0) continue;
    if (!Number.isInteger(value)) {
      throw structuredError(
        "OP_INVALID",
        `interval(duration): ${key} must be an integer; got ${JSON.stringify(value)}`
      );
    }
    normalized[key] = value;
  }
  for (const key of Object.keys(duration)) {
    if (!durationFields.includes(key)) {
      throw structuredError(
        "OP_INVALID",
        `interval(duration): unknown duration field ${JSON.stringify(key)}`
      );
    }
  }
  if (Object.keys(normalized).length === 0) {
    throw structuredError("OP_INVALID", "interval(duration): at least one duration field is required");
  }
  return normalized;
}
var ExprChainImpl = class {
  __node;
  constructor(node) {
    this.__node = node;
  }
  bin(op, x) {
    return chain({ node: "binOp", op, lhs: this.__node, rhs: exprArg(x) });
  }
  eq(x) {
    if (x === null) {
      throw structuredError("OP_INVALID", "eq(null) is always UNKNOWN in SQL \u2014 use isNull()");
    }
    return this.bin("eq", x);
  }
  ne(x) {
    if (x === null) {
      throw structuredError("OP_INVALID", "ne(null) is always UNKNOWN in SQL \u2014 use isNotNull()");
    }
    return this.bin("ne", x);
  }
  lt(x) {
    return this.bin("lt", x);
  }
  le(x) {
    return this.bin("le", x);
  }
  gt(x) {
    return this.bin("gt", x);
  }
  ge(x) {
    return this.bin("ge", x);
  }
  and(...es) {
    let acc = this.__node;
    for (const e of es) acc = { node: "binOp", op: "and", lhs: acc, rhs: exprArg(e) };
    return chain(acc);
  }
  or(...es) {
    let acc = this.__node;
    for (const e of es) acc = { node: "binOp", op: "or", lhs: acc, rhs: exprArg(e) };
    return chain(acc);
  }
  not() {
    return chain({ node: "unaryOp", op: "not", operand: this.__node });
  }
  add(x) {
    return this.bin("add", x);
  }
  sub(x) {
    return this.bin("sub", x);
  }
  mul(x) {
    return this.bin("mul", x);
  }
  div(x) {
    return this.bin("div", x);
  }
  concat(...parts) {
    let acc = this.__node;
    for (const p of parts) acc = { node: "binOp", op: "concat", lhs: acc, rhs: exprArg(p) };
    return chain(acc);
  }
  isNull() {
    return chain({ node: "unaryOp", op: "isNull", operand: this.__node });
  }
  isNotNull() {
    return chain({ node: "unaryOp", op: "isNotNull", operand: this.__node });
  }
  isTrue() {
    return chain({ node: "unaryOp", op: "isTrue", operand: this.__node });
  }
  isFalse() {
    return chain({ node: "unaryOp", op: "isFalse", operand: this.__node });
  }
  cast(args) {
    return chain({ node: "cast", operand: this.__node, target: castTarget(args) });
  }
  // Portable predicate nodes. `between`/`like` render identical syntax on
  // all three dialects; `distinctFrom` is portably named but per-dialect rendered
  // (PG/SQLite `IS DISTINCT FROM` vs MySQL `NOT (x <=> y)`) — the engine owns it.
  between(low, high) {
    return chain({ node: "between", operand: this.__node, low: exprArg(low), high: exprArg(high) });
  }
  like(pattern) {
    return chain({ node: "like", operand: this.__node, pattern: exprArg(pattern) });
  }
  "in"(values) {
    return chain({
      node: "inList",
      expr: this.__node,
      elems: scalarLiteralArray(values, ".in(values)"),
      negated: false
    });
  }
  notIn(values) {
    return chain({
      node: "inList",
      expr: this.__node,
      elems: scalarLiteralArray(values, ".notIn(values)"),
      negated: true
    });
  }
  distinctFrom(x) {
    return chain({ node: "distinctFrom", left: this.__node, right: exprArg(x) });
  }
  // PG-first chain operators. Same IR nodes as the old vendor helpers;
  // the dialect gate lives in the Rust validator (fail-closed off-target).
  regex(pattern) {
    return chain({ node: "pgRegexMatch", expr: this.__node, pattern: pgRegexPattern(pattern) });
  }
  columnSize() {
    return chain({ node: "pgColumnSize", expr: this.__node });
  }
  lower() {
    return chain({ node: "fnCall", fn: "lower", args: [this.__node] });
  }
  upper() {
    return chain({ node: "fnCall", fn: "upper", args: [this.__node] });
  }
  trim() {
    return chain({ node: "fnCall", fn: "trim", args: [this.__node] });
  }
  length() {
    return chain({ node: "fnCall", fn: "length", args: [this.__node] });
  }
  abs() {
    return chain({ node: "fnCall", fn: "abs", args: [this.__node] });
  }
  coalesce(...rest) {
    return chain({ node: "fnCall", fn: "coalesce", args: [this.__node, ...rest.map(exprArg)] });
  }
  nullif(b) {
    return chain({ node: "fnCall", fn: "nullif", args: [this.__node, exprArg(b)] });
  }
  mod(b) {
    return chain({ node: "fnCall", fn: "mod", args: [this.__node, exprArg(b)] });
  }
  round(n) {
    return chain({
      node: "fnCall",
      fn: "round",
      args: n === void 0 ? [this.__node] : [this.__node, exprArg(n)]
    });
  }
  floor() {
    return chain({ node: "fnCall", fn: "floor", args: [this.__node] });
  }
  ceil() {
    return chain({ node: "fnCall", fn: "ceil", args: [this.__node] });
  }
  substr(start, len) {
    return chain({
      node: "fnCall",
      fn: "substr",
      args: len === void 0 ? [this.__node, exprArg(start)] : [this.__node, exprArg(start), exprArg(len)]
    });
  }
  replace(from, to) {
    return chain({ node: "fnCall", fn: "replace", args: [this.__node, exprArg(from), exprArg(to)] });
  }
  extract(field) {
    const f = pgExtractField(field);
    return chain({
      node: portableExtractFieldSet.has(f) ? "extract" : "pgExtract",
      field: f,
      from: this.__node
    });
  }
  splitPart(delim, n) {
    splitPartGrammarLint(delim, n);
    return chain({
      node: "fnSynth",
      fn: "splitPart",
      args: [this.__node, { node: "literal", value: delim }, { node: "literal", value: n }]
    });
  }
  count(opts) {
    return aggNode("count", this.__node, opts);
  }
  sum(opts) {
    return aggNode("sum", this.__node, opts);
  }
  avg(opts) {
    return aggNode("avg", this.__node, opts);
  }
  min(opts) {
    return aggNode("min", this.__node, opts);
  }
  max(opts) {
    return aggNode("max", this.__node, opts);
  }
  stringAgg(delimiter) {
    return aggNode("stringAgg", this.__node, { delimiter });
  }
  arrayAgg() {
    return aggNode("arrayAgg", this.__node);
  }
  boolAnd() {
    return aggNode("boolAnd", this.__node);
  }
  boolOr() {
    return aggNode("boolOr", this.__node);
  }
};
function check(name, expr) {
  requireString(name, "check(name, expr)");
  if (typeof expr !== "function") {
    throw structuredError("OP_INVALID", "check(name, expr): expr must be a (col) => Expr callback");
  }
  return { name, expr };
}
function lit(value) {
  return chain({ node: "literal", value: toIrScalar(value) });
}
function concatWs(sep, ...parts) {
  return chain({ node: "fnSynth", fn: "concatWs", args: [exprArg(sep), ...parts.map(exprArg)] });
}
function countStar() {
  return aggNode("count", void 0);
}
function dialect(legs) {
  if (legs === null || typeof legs !== "object") {
    throw structuredError(
      "OP_INVALID",
      "dialect(legs): legs must be an object with default/pg/sqlite/mysql expression or op thunk legs"
    );
  }
  const ordered = ["default", "pg", "sqlite", "mysql"];
  const present = ordered.map((leg) => [leg, legs[leg]]).filter(([, value]) => value !== void 0);
  if (present.length === 0) {
    throw structuredError(
      "OP_INVALID",
      "dialect(legs): at least one leg (default/pg/sqlite/mysql) must be present"
    );
  }
  const isOpThunk = (value) => typeof value === "function" && nativeDbExprNode(value) === void 0;
  const firstIsThunk = isOpThunk(present[0][1]);
  for (const [, value] of present.slice(1)) {
    if (isOpThunk(value) !== firstIsThunk) {
      throw structuredError(
        "OP_INVALID",
        "dialect(legs): cannot mix op thunk legs with expression legs"
      );
    }
  }
  if (firstIsThunk) {
    const node2 = {};
    for (const [leg, value] of present) {
      const thunk = value;
      const rec = recorder();
      const start = rec.ops.length;
      thunk();
      node2[leg] = rec.ops.splice(start);
    }
    emitDialectal(node2);
    return;
  }
  const node = { node: "dialect" };
  for (const [leg, value] of present) {
    node[leg] = exprArg(value);
  }
  return chain(node);
}
function aggNode(func, expr, opts) {
  const node = { node: "agg", func };
  if (expr !== void 0) node.arg = exprArg(expr);
  if (opts && opts.delimiter !== void 0) node.delimiter = exprArg(opts.delimiter);
  if (opts && opts.distinct === true) node.distinct = true;
  return chain(node);
}
function caseExpr(args) {
  const shape = "col.case({ branches: [{ when, then }], else? })";
  if (!isPlainObject(args)) {
    throw structuredError("OP_INVALID", `${shape}: args must be an object`);
  }
  const branches = args.branches;
  if (!Array.isArray(branches) || branches.length === 0) {
    throw structuredError(
      "OP_INVALID",
      `${shape}: branches must be a non-empty array of { when, then } objects`
    );
  }
  const node = {
    node: "case",
    branches: branches.map((branch, i) => {
      if (!isPlainObject(branch) || !Object.prototype.hasOwnProperty.call(branch, "when") || !Object.prototype.hasOwnProperty.call(branch, "then")) {
        throw structuredError(
          "OP_INVALID",
          `${shape}: branches[${i}] must be an object with when and then`
        );
      }
      return { when: exprArg(branch.when), then: exprArg(branch.then) };
    })
  };
  if (args.else !== void 0) node.else = exprArg(args.else);
  return chain(node);
}
function makeColumnAccessor() {
  return ((first, second) => {
    if (second === void 0) {
      requireString(first, 'col("name")');
      return chain({ node: "colRef", name: first });
    }
    requireString(first, 'col("table", "col")');
    requireString(second, 'col("table", "col")');
    return chain({ node: "colRef", table: first, name: second });
  });
}
function immutableExprBuilder() {
  const c = makeColumnAccessor();
  c.case = caseExpr;
  return Object.freeze(c);
}
function checkBuilder() {
  const c = makeColumnAccessor();
  c.case = caseExpr;
  return Object.freeze(c);
}
function domainValueBuilder() {
  const v = chain({ node: "colRef", name: "VALUE" });
  v.case = caseExpr;
  return Object.freeze(v);
}
function makeBuilder() {
  const c = makeColumnAccessor();
  c.case = caseExpr;
  return c;
}
var cCase = caseExpr;
function resolveExpr(slot) {
  if (slot === void 0 || slot === null) return void 0;
  rejectNestedPerRowGeneratorValues(slot);
  if (typeof slot === "function") return exprArg(slot(makeBuilder()));
  if (slot instanceof ExprChainImpl) return slot.__node;
  if (slot && typeof slot === "object" && typeof slot.node === "string") return slot;
  throw structuredError("OP_INVALID", "expression slot must be a (col) => Expr callback or a built expression");
}
function resolveImmutableExpr(slot, position) {
  if (slot === void 0 || slot === null) return void 0;
  let resolved;
  if (typeof slot === "function") {
    resolved = exprArg(slot(immutableExprBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof slot.node === "string") {
    resolved = slot;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (col) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position);
  return resolved;
}
function resolveCheckExpr(slot, position) {
  if (slot === void 0 || slot === null) return void 0;
  let resolved;
  if (typeof slot === "function") {
    resolved = exprArg(slot(checkBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof slot.node === "string") {
    resolved = slot;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (col) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position, { allowPgImmutable: true });
  return resolved;
}
function validateDomainCheckColRefs(expr, position) {
  const walk = (value) => {
    if (Array.isArray(value)) {
      value.forEach(walk);
      return;
    }
    if (!isPlainObject(value)) return;
    if (value.node === "colRef") {
      const table2 = value.table;
      if (value.name !== "VALUE" || table2 !== void 0 && table2 !== null) {
        throw structuredError(
          "OP_INVALID",
          `${position} may reference only the domain VALUE pseudo-column; non-VALUE colRef nodes are not valid in domain SQL`
        );
      }
    }
    Object.values(value).forEach(walk);
  };
  walk(expr);
}
function resolveDomainCheck(slot, position) {
  if (slot === void 0 || slot === null) return void 0;
  let resolved;
  if (typeof slot === "function") {
    resolved = exprArg(slot(domainValueBuilder()));
  } else if (slot instanceof ExprChainImpl) {
    resolved = slot.__node;
  } else if (slot && typeof slot === "object" && typeof slot.node === "string") {
    resolved = slot;
  } else {
    throw structuredError("OP_INVALID", `${position} must be a (v) => Expr callback or a built expression`);
  }
  validateImmutableExpr(resolved, position, { allowPgImmutable: true });
  validateDomainCheckColRefs(resolved, position);
  return resolved;
}
var resolveTableCheckExpr = (slot, position) => resolveCheckExpr(slot, position);
function rejectImmutableExpr(position, reason) {
  throw structuredError(
    "OP_INVALID",
    `${position} must use only immutable expressions: column refs, literals, CASE, operators, and immutable scalar chain helpers (${IMMUTABLE_HELPERS}); ${reason}`
  );
}
function validateImmutableExpr(expr, position, opts = {}) {
  rejectNestedPerRowGeneratorValues(expr);
  const rejectPgNode = (nodeName) => {
    rejectImmutableExpr(position, `${nodeName} is PG-vendor and non-portable`);
  };
  const walk = (node) => {
    if (!node || typeof node !== "object" || typeof node.node !== "string") {
      rejectImmutableExpr(position, "found a non-expression value");
    }
    const n = node;
    switch (n.node) {
      case "colRef":
      case "literal":
        return;
      case "uuidV4":
        rejectImmutableExpr(position, "uuidV4 is volatile");
      case "uuidV7":
        rejectImmutableExpr(position, "uuidV7 is volatile");
      case "agg":
        if (n.arg !== void 0 && n.arg !== null) walk(n.arg);
        if (n.delimiter !== void 0 && n.delimiter !== null) walk(n.delimiter);
        return;
      case "fnCall": {
        if (n.fn === "currentSetting" || n.fn === "currentUser") {
          rejectImmutableExpr(position, `${String(n.fn)} is PG-vendor and non-portable`);
        }
        if (typeof n.fn !== "string" || !IMMUTABLE_SCALAR_FNS.has(n.fn)) {
          rejectImmutableExpr(position, `function ${JSON.stringify(n.fn)} is not an immutable scalar chain helper`);
        }
        if (!Array.isArray(n.args)) {
          rejectImmutableExpr(position, "function expression args must be an array");
        }
        n.args.forEach(walk);
        return;
      }
      case "fnSynth": {
        if (n.fn === "now") {
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
        if (n.else !== void 0 && n.else !== null) walk(n.else);
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
        for (const leg of ["default", "pg", "sqlite", "mysql"]) {
          if (n[leg] !== void 0 && n[leg] !== null) walk(n[leg]);
        }
        return;
      default:
        rejectImmutableExpr(position, `unsupported expression node ${JSON.stringify(n.node)}`);
    }
  };
  walk(expr);
}
function resolveSetValue(value) {
  const synth = nativeDbExprNode(value);
  if (synth !== void 0) return synth;
  if (typeof value === "function") return resolveExpr(value);
  return toIrValue(value);
}
function resolveSet(set) {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column \u2192 DML value");
  }
  const out = {};
  for (const col of Object.keys(set)) setOwn(out, col, resolveSetValue(set[col]));
  return out;
}
function resolveBackfillSetValue(value) {
  const generator = perRowGeneratorOf(value);
  if (generator !== void 0) return { perRow: generator };
  return resolveSetValue(value);
}
function resolveBackfillSet(set) {
  if (!set || typeof set !== "object") {
    throw structuredError("OP_INVALID", "`set` must be an object of column \u2192 backfill value");
  }
  const out = {};
  for (const col of Object.keys(set)) setOwn(out, col, resolveBackfillSetValue(set[col]));
  return out;
}
function ifNotExistsGuard(v) {
  return v ? "ifNotExists" : void 0;
}
function ifExistsGuard(v) {
  return v ? "ifExists" : void 0;
}
function stringArray(values, what) {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be a string[]`);
  }
  for (const v of values) requireString(v, what);
  return [...values];
}
var PARTITION_BOUND_SENTINEL = "__zeroMigratePartitionBound";
var minValue = Object.freeze({ [PARTITION_BOUND_SENTINEL]: "minValue" });
var maxValue = Object.freeze({ [PARTITION_BOUND_SENTINEL]: "maxValue" });
function partitionSpecToIr(spec, what) {
  if (spec === void 0) return void 0;
  if (!spec || typeof spec !== "object") {
    throw structuredError("OP_INVALID", `${what} must be exactly one of { range }, { list }, or { hash }`);
  }
  const shape = spec;
  const variants = (shape.range !== void 0 ? 1 : 0) + (shape.list !== void 0 ? 1 : 0) + (shape.hash !== void 0 ? 1 : 0);
  if (variants !== 1) {
    throw structuredError("OP_INVALID", `${what} must be exactly one of { range }, { list }, or { hash }`);
  }
  const collapse = shape.whenUnsupported === void 0 ? void 0 : shape.whenUnsupported;
  if (collapse !== void 0 && collapse !== "collapse") {
    throw structuredError("OP_INVALID", `${what}.whenUnsupported must be "collapse" when present`);
  }
  const affirmation = { collapse: collapse === "collapse" };
  if (shape.range !== void 0) return { kind: "range", columns: stringArray(shape.range, `${what}.range`), ...affirmation };
  if (shape.list !== void 0) return { kind: "list", columns: stringArray(shape.list, `${what}.list`), ...affirmation };
  return { kind: "hash", columns: stringArray(shape.hash, `${what}.hash`), ...affirmation };
}
function requireU32(v, what) {
  if (v === void 0) return void 0;
  if (typeof v !== "number" || !Number.isSafeInteger(v) || v < 0 || v > 4294967295) {
    throw structuredError("OP_INVALID", `${what} must be a u32 integer; got ${v}`);
  }
  return v;
}
function partitionBoundValueToIr(value, what) {
  if (value === minValue) return { kind: "minValue" };
  if (value === maxValue) return { kind: "maxValue" };
  if (typeof value === "string") return { kind: "string", value };
  if (typeof value === "number" && Number.isSafeInteger(value)) {
    return { kind: "int", value };
  }
  throw structuredError(
    "OP_INVALID",
    `${what} must be a string, JS safe integer, minValue, or maxValue`
  );
}
function partitionBoundListToIr(values, what) {
  if (!Array.isArray(values)) {
    throw structuredError("OP_INVALID", `${what} must be an array`);
  }
  return values.map((value, i) => partitionBoundValueToIr(value, `${what}[${i}]`));
}
function partitionBoundToIr(args) {
  if (!args || typeof args !== "object") {
    throw structuredError(
      "OP_INVALID",
      "table(parent).partition(name).create(bound) needs a bounds object"
    );
  }
  const bounds = args;
  const hasRange = bounds.from !== void 0 || bounds.to !== void 0;
  const hasList = bounds.in !== void 0;
  const hasHash = bounds.modulus !== void 0 || bounds.remainder !== void 0;
  const hasDefault = bounds.default !== void 0;
  const variantCount = (hasRange ? 1 : 0) + (hasList ? 1 : 0) + (hasHash ? 1 : 0) + (hasDefault ? 1 : 0);
  if (variantCount !== 1) {
    throw structuredError(
      "OP_INVALID",
      "partition bounds must be exactly one of { from, to }, { in }, { modulus, remainder }, or { default: true }"
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
      to: partitionBoundListToIr(bounds.to, "partition bounds.to")
    };
  }
  if (hasList) {
    return {
      kind: "list",
      values: partitionBoundListToIr(bounds.in, "partition bounds.in")
    };
  }
  return {
    kind: "hash",
    modulus: requireU32(bounds.modulus, "partition bounds.modulus"),
    remainder: requireU32(bounds.remainder, "partition bounds.remainder")
  };
}
function indexIncludeToIr(include) {
  if (include === void 0) return void 0;
  const cols = stringArray(include, "index include");
  return cols.length === 0 ? void 0 : cols;
}
function indexWithToIr(params) {
  if (params === void 0) return void 0;
  if (!params || typeof params !== "object") {
    throw structuredError("OP_INVALID", "index with(...) must be an object");
  }
  const withParams = compact({
    pagesPerRange: requireU32(params.pagesPerRange, "index with.pagesPerRange"),
    fillfactor: requireU32(params.fillfactor, "index with.fillfactor")
  });
  return Object.keys(withParams).length === 0 ? void 0 : withParams;
}
function recordCreateEnum(name, args) {
  requireString(name, "enumType(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "enumType(name).create({ values, ... }) needs an object");
  }
  const enumValues = stringArray(args.values, "enumType(name).create({ values })");
  if (enumValues.length === 0) {
    throw structuredError(
      "OP_INVALID",
      "enumType(name).create({ values }): values must be a non-empty string[] (an empty enum renders invalid SQL on MySQL/SQLite)"
    );
  }
  emitCreateEnum({
    name,
    schema: args.schema,
    values: enumValues
  });
}
function recordDropEnum(name, args = {}) {
  requireString(name, "enumType(name).drop()");
  emitDropEnum({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordCreateDomain(name, args) {
  requireString(name, "domain(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "domain(name).create({ as, ... }) needs an object");
  }
  if (args.notNull !== void 0 && typeof args.notNull !== "boolean") {
    throw structuredError("OP_INVALID", "domain(name).create({ notNull }): notNull must be a boolean");
  }
  emitCreateDomain({
    name,
    schema: args.schema,
    as: colTypeOf(args.as),
    check: resolveDomainCheck(args.check, "domain(name).create({ check })"),
    default: args.default === void 0 ? void 0 : toIrDefault(args.default),
    notNull: args.notNull
  });
}
function recordDropDomain(name, args = {}) {
  requireString(name, "domain(name).drop()");
  emitDropDomain({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordCreateSequence(name, args = {}) {
  requireString(name, "sequence(name)");
  if (args === null || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).create(args) needs an object");
  }
  const minValue2 = requireNullableSafeI64(args.minValue, "sequence.create({ minValue })");
  const maxValue2 = requireNullableSafeI64(args.maxValue, "sequence.create({ maxValue })");
  requireSequenceBounds(minValue2, maxValue2, "sequence.create(args)");
  const asType = args.as === void 0 ? void 0 : colTypeOf(args.as);
  if (asType !== void 0 && !SEQUENCE_AS_TYPES.includes(asType)) {
    throw structuredError(
      "OP_INVALID",
      `sequence.create({ as }): as must be one of ${SEQUENCE_AS_TYPES.join(" | ")}; got ${JSON.stringify(asType)}`
    );
  }
  emitCreateSequence({
    name,
    schema: args.schema,
    as: asType,
    increment: requireSequenceIncrement(args.increment, "sequence.create({ increment })"),
    start: requireSafeI64(args.start, "sequence.create({ start })"),
    minValue: minValue2,
    maxValue: maxValue2,
    cache: requireSequenceCache(args.cache, "sequence.create({ cache })"),
    cycle: args.cycle,
    ownedBy: args.ownedBy
  });
}
function recordAlterSequence(name, args) {
  requireString(name, "sequence(name)");
  if (!args || typeof args !== "object") {
    throw structuredError("OP_INVALID", "sequence(name).alter(args) needs an object");
  }
  const minValue2 = requireNullableSafeI64(args.minValue, "sequence.alter({ minValue })");
  const maxValue2 = requireNullableSafeI64(args.maxValue, "sequence.alter({ maxValue })");
  requireSequenceBounds(minValue2, maxValue2, "sequence.alter(args)");
  emitAlterSequence({
    name,
    schema: args.schema,
    increment: requireSequenceIncrement(args.increment, "sequence.alter({ increment })"),
    restart: requireNullableSafeI64(args.restart, "sequence.alter({ restart })"),
    minValue: minValue2,
    maxValue: maxValue2,
    cache: requireSequenceCache(args.cache, "sequence.alter({ cache })"),
    cycle: args.cycle,
    ownedBy: args.ownedBy
  });
}
function recordDropSequence(name, args = {}) {
  requireString(name, "sequence(name)");
  emitDropSequence({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function requirePlainArgs(args, what) {
  if (args === null || typeof args !== "object") {
    throw structuredError("OP_INVALID", `${what} needs an object`);
  }
}
function recordCreateSchema(name, args = {}) {
  requireString(name, "schema(name)");
  requirePlainArgs(args, "schema(name).create(args)");
  emitCreateSchema({
    name,
    ifNotExists: args.ifNotExists,
    authorization: args.authorization
  });
}
function recordDropSchema(name, args = {}) {
  requireString(name, "schema(name)");
  requirePlainArgs(args, "schema(name).drop(args)");
  emitDropSchema({
    name,
    ifExists: args.ifExists,
    cascade: args.cascade
  });
}
function recordCreateExtension(name, args = {}) {
  requireString(name, "extension(name)");
  requirePlainArgs(args, "extension(name).create(args)");
  emitCreateExtension({
    name,
    ifNotExists: args.ifNotExists,
    schema: args.schema
  });
}
function recordDropExtension(name, args = {}) {
  requireString(name, "extension(name)");
  requirePlainArgs(args, "extension(name).drop(args)");
  emitDropExtension({
    name,
    ifExists: args.ifExists
  });
}
function recordCreateRole(name, args = {}) {
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
    ifNotExists: args.ifNotExists
  });
}
function recordSetRoleOptions(name, args) {
  requireString(name, "role(name)");
  requirePlainArgs(args, "role(name).setOptions(args)");
  emitAlterRole({
    name,
    setSearchPath: args.setSearchPath,
    resetSearchPath: args.resetSearchPath
  });
}
function recordDropRole(name, args = {}) {
  requireString(name, "role(name)");
  requirePlainArgs(args, "role(name).drop(args)");
  emitDropRole({
    name,
    ifExists: args.ifExists
  });
}
function recordComment(target, text) {
  if (text !== null && typeof text !== "string") {
    throw structuredError("OP_INVALID", "comment text must be a string or null");
  }
  emitComment({
    target: commentTargetToIr(target),
    comment: text === null ? void 0 : text
  });
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
        name: target.name
      });
    default:
      throw structuredError("OP_INVALID", `unsupported comment target kind ${target.kind}`);
  }
}
function recordCreateTable(name, args, checkExprResolver = resolveTableCheckExpr) {
  const cols = [];
  const constraints = [];
  const indexes = [];
  const pkCols = [];
  const columnNames = Object.keys(args.columns);
  for (const colName of columnNames) {
    const def = args.columns[colName];
    if (!isColumnDef(def)) {
      throw structuredError("OP_INVALID", `create column "${colName}" must be a t.* ColumnDef`);
    }
    cols.push(def.__toIrColumn(colName));
    if (def._primaryKey) pkCols.push(colName);
  }
  const tablePrimaryKey = args.primaryKey;
  if (tablePrimaryKey !== void 0 && tablePrimaryKey !== null) {
    if (!Array.isArray(tablePrimaryKey)) {
      throw structuredError(
        "PRIMARY_KEY_INVALID",
        `create table "${name}" primaryKey must be null or an ordered column-name array`
      );
    }
    if (tablePrimaryKey.length === 0) {
      throw structuredError(
        "PRIMARY_KEY_INVALID",
        `create table "${name}" primaryKey cannot be empty; omit it for no primary key`
      );
    }
    const seen = /* @__PURE__ */ new Set();
    const knownColumns2 = new Set(columnNames);
    for (const column of tablePrimaryKey) {
      if (typeof column !== "string") {
        throw structuredError(
          "PRIMARY_KEY_INVALID",
          `create table "${name}" primaryKey must contain only column names`
        );
      }
      if (seen.has(column)) {
        throw structuredError(
          "PRIMARY_KEY_INVALID",
          `create table "${name}" primaryKey names column "${column}" more than once`
        );
      }
      if (!knownColumns2.has(column)) {
        throw structuredError(
          "PRIMARY_KEY_INVALID",
          `create table "${name}" primaryKey names unknown column "${column}"`
        );
      }
      seen.add(column);
    }
  }
  if (pkCols.length > 1) {
    throw structuredError(
      "PRIMARY_KEY_INVALID",
      `create table "${name}" marks multiple columns with .primaryKey(); author a composite primary key solely with the ordered table-level primaryKey array`,
      { columns: pkCols }
    );
  }
  if (pkCols.length === 1 && tablePrimaryKey !== void 0) {
    const columnPrimaryKey = pkCols[0];
    const declarationsMatch = Array.isArray(tablePrimaryKey) && tablePrimaryKey.length === 1 && tablePrimaryKey[0] === columnPrimaryKey;
    if (!declarationsMatch) {
      throw structuredError(
        "PRIMARY_KEY_INVALID",
        `create table "${name}" declares column "${columnPrimaryKey}" with .primaryKey() and a conflicting table-level primaryKey; use one consistent single-column declaration`,
        { column: columnPrimaryKey, primaryKey: tablePrimaryKey }
      );
    }
  }
  const primaryKey = args.primaryKey !== void 0 ? args.primaryKey : pkCols.length ? pkCols : void 0;
  for (const uq of args.uniques ?? []) {
    constraints.push(compact({ name: uq.name, kind: { kind: "unique", columns: uq.columns } }));
  }
  for (const ck of args.checks ?? []) {
    constraints.push(compact({
      name: ck.name,
      kind: { kind: "check", expr: checkExprResolver(ck.expr, "check constraint") }
    }));
  }
  for (const exclusion of args.exclusions ?? []) {
    constraints.push(exclusionConstraintFromSpec(exclusion));
  }
  if (args.foreignKeys !== void 0 && !Array.isArray(args.foreignKeys)) {
    throw structuredError("OP_INVALID", `create table "${name}" foreignKeys must be an array`);
  }
  const knownColumns = new Set(columnNames);
  const foreignKeyNames = /* @__PURE__ */ new Set();
  for (const [position, fkSpec] of (args.foreignKeys ?? []).entries()) {
    requirePlainObject(fkSpec, `create table "${name}" foreignKeys[${position}]`);
    requireNonEmptyString(
      fkSpec.name,
      `create table "${name}" foreignKeys[${position}].name`
    );
    if (foreignKeyNames.has(fkSpec.name)) {
      throw structuredError(
        "OP_INVALID",
        `create table "${name}" foreignKeys names constraint ${JSON.stringify(fkSpec.name)} more than once`
      );
    }
    foreignKeyNames.add(fkSpec.name);
    requireOrderedColumns(
      fkSpec.columns,
      `create table "${name}" foreign key ${JSON.stringify(fkSpec.name)} columns`
    );
    for (const column of fkSpec.columns) {
      if (!knownColumns.has(column)) {
        throw structuredError(
          "OP_INVALID",
          `create table "${name}" foreign key ${JSON.stringify(fkSpec.name)} names unknown local column ${JSON.stringify(column)}`
        );
      }
    }
    constraints.push(
      fkConstraintFromSpec({
        name: fkSpec.name,
        columns: fkSpec.columns,
        references: fkSpec.references,
        onDelete: fkSpec.onDelete,
        onUpdate: fkSpec.onUpdate,
        deferrable: fkSpec.deferrable,
        initiallyDeferred: fkSpec.initiallyDeferred,
        schema: args.schema
      })
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
        where: resolveImmutableExpr(idx.where, "partial index predicate"),
        include: indexIncludeToIr(idx.include),
        with: indexWithToIr(idx.with),
        only: requireOptionalBoolean(idx.only, "index only"),
        nullsNotDistinct: requireOptionalBoolean(idx.nullsNotDistinct, "index nullsNotDistinct")
      })
    );
  }
  emitCreateTable({
    name,
    columns: cols,
    primaryKey,
    constraints: constraints.length ? constraints : void 0,
    indexes: indexes.length ? indexes : void 0,
    partitionBy: partitionSpecToIr(args.partitionBy, "create({ partitionBy })"),
    runtimeOptions: runtimeOptionsFromCreateArgs(args),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordCreatePartition(name, parent, bounds, args) {
  emitCreatePartition({
    name,
    of: parent,
    bounds,
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordAttachPartition(parent, name, bound, args) {
  emitAttachPartition({
    parent,
    name,
    bound,
    schema: args.schema
  });
}
function recordDetachPartition(parent, name, args) {
  emitDetachPartition({
    parent,
    name,
    schema: args.schema,
    concurrently: args.concurrently
  });
}
function recordDropPartition(parent, name, args) {
  emitDropPartition({
    parent,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
    cascade: args.cascade
  });
}
function recordSetTableOptions(table2, args) {
  emitSetTableOptions({
    table: table2,
    options: runtimeOptionsPatchFromArgs(args),
    schema: args.schema
  });
}
function recordSetRls(table2, args) {
  const enabled = requireOptionalBoolean(args.enabled, ".setRls({ enabled })");
  const forced = requireOptionalBoolean(args.forced, ".setRls({ forced })");
  if (enabled === void 0 && forced === void 0) {
    throw structuredError("OP_INVALID", ".setRls needs at least one of { enabled, forced }");
  }
  emitSetRls({
    table: table2,
    schema: args.schema,
    enabled,
    forced
  });
}
function recordDropTable(table2, args) {
  emitDropTable({
    table: table2,
    cascade: args.cascade,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordRenameTable(table2, to, args) {
  emitRenameTable({
    table: table2,
    to,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordAddColumn(table2, column, type, args) {
  emitAddColumn({
    table: table2,
    column,
    ...type.__toAddColumnTail(),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
  if (type._unique) {
    emitAddColumnUnique({
      table: table2,
      constraint: { kind: { kind: "unique", columns: [column] } },
      schema: args.schema,
      existenceGuard: ifNotExistsGuard(args.ifNotExists)
    });
  }
}
function recordDropColumn(table2, column, args) {
  emitDropColumn({
    table: table2,
    column,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordRenameColumn(table2, from, to, type, args) {
  emitRenameColumn({
    table: table2,
    from,
    to,
    type: colTypeOf(type),
    schema: args.schema
  });
}
function recordSetColumnType(table2, name, change) {
  requireColumnDef(change.to, ".column(name).setType({ to })");
  emitSetColumnType({
    table: table2,
    column: name,
    toType: colTypeOf(change.to),
    using: resolveExpr(change.using),
    schema: change.schema
  });
}
function recordSetColumnNotNull(table2, name, args) {
  emitSetColumnNotNull({ table: table2, column: name, schema: args.schema });
}
function recordDropColumnNotNull(table2, name, args) {
  emitDropColumnNotNull({ table: table2, column: name, schema: args.schema });
}
function recordSetColumnDefault(table2, name, value, args) {
  emitSetColumnDefault({
    table: table2,
    column: name,
    value: toIrDefault(value),
    schema: args.schema
  });
}
function recordDropColumnDefault(table2, name, args) {
  emitDropColumnDefault({ table: table2, column: name, schema: args.schema });
}
function fkConstraintFromSpec(spec) {
  if (!spec || typeof spec !== "object") {
    throw structuredError("OP_INVALID", ".foreignKey(name).add needs { columns, references:{ table, columns } }");
  }
  requireNonEmptyString(spec.name, "foreign key name");
  requireOrderedColumns(spec.columns, "foreign key columns");
  requirePlainObject(spec.references, "foreign key references");
  requireNonEmptyString(spec.references.table, "foreign key references.table");
  requireOrderedColumns(spec.references.columns, "foreign key references.columns");
  if (spec.columns.length !== spec.references.columns.length) {
    throw structuredError(
      "OP_INVALID",
      `foreign key local and referenced columns must have equal arity; got ${spec.columns.length} and ${spec.references.columns.length}`,
      { localArity: spec.columns.length, referencedArity: spec.references.columns.length }
    );
  }
  if (spec.references.schema !== void 0) {
    requireNonEmptyString(spec.references.schema, "foreign key references.schema");
    if (spec.schema === void 0) {
      throw structuredError(
        "OP_INVALID",
        "foreign key references.schema requires an explicit matching table schema; the frozen IR cannot prove an implicit schema or represent a cross-schema FK"
      );
    }
    if (spec.references.schema !== spec.schema) {
      throw structuredError(
        "OP_INVALID",
        "foreign key references.schema must match the table schema; cross-schema FKs are not representable in the frozen IR"
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
      onDelete: requireReferenceAction(spec.onDelete, "foreign key onDelete"),
      onUpdate: requireReferenceAction(spec.onUpdate, "foreign key onUpdate"),
      deferrable: requireOptionalBoolean(spec.deferrable, "foreign key deferrable"),
      initiallyDeferred: requireOptionalBoolean(
        spec.initiallyDeferred,
        "foreign key initiallyDeferred"
      ),
      // PG-only online constraint adoption; refused off Postgres at validate.
      notValid: requireOptionalBoolean(spec.notValid, "foreign key notValid")
    })
  });
}
function recordAddForeignKey(table2, name, args) {
  emitAddForeignKey({
    table: table2,
    constraint: fkConstraintFromSpec({
      name,
      columns: args.columns,
      references: args.references,
      onDelete: args.onDelete,
      onUpdate: args.onUpdate,
      deferrable: args.deferrable,
      initiallyDeferred: args.initiallyDeferred,
      notValid: args.notValid,
      schema: args.schema
    }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordAddUnique(table2, name, args) {
  if (!Array.isArray(args.columns)) {
    throw structuredError("OP_INVALID", ".unique(name).add needs { columns: string[] }");
  }
  emitAddUnique({
    table: table2,
    constraint: compact({ name, kind: { kind: "unique", columns: args.columns } }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordAddCheck(table2, name, args, checkExprResolver = resolveTableCheckExpr) {
  if (!args || args.expr === void 0) {
    throw structuredError("OP_INVALID", ".check(name).add needs { expr: (col) => Expr }");
  }
  emitAddCheck({
    table: table2,
    constraint: compact({
      name,
      // `notValid` is PG-only online constraint adoption; compacted out when absent
      // so an ordinary CHECK is byte-identical to the pre-slice wire image.
      kind: compact({
        kind: "check",
        expr: checkExprResolver(args.expr, "check constraint"),
        notValid: args.notValid
      })
    }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordValidateConstraint(table2, name, args) {
  emitValidateConstraint({
    table: table2,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function exclusionConstraintFromSpec(spec) {
  if (!spec || typeof spec !== "object" || !Array.isArray(spec.elements)) {
    throw structuredError(
      "OP_INVALID",
      ".exclusion(name).add needs { elements: [{ target, operator }], ... }"
    );
  }
  return compact({
    name: spec.name,
    kind: compact({
      kind: "exclusion",
      usingMethod: spec.using,
      elements: spec.elements.map(exclusionElementToIr),
      wherePredicate: resolveImmutableExpr(
        spec.where,
        "exclusion predicate"
      ),
      deferrable: spec.deferrable,
      initiallyDeferred: spec.initiallyDeferred
    })
  });
}
function exclusionElementToIr(element) {
  if (!element || typeof element !== "object") {
    throw structuredError(
      "OP_INVALID",
      "exclusion element must be { target, operator }"
    );
  }
  return {
    target: exclusionTargetToIr(element.target),
    operator: element.operator
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
  return {
    kind: "expr",
    expr
  };
}
function indexElementToIr(element) {
  if (typeof element === "string") {
    requireString(element, "index element column");
    return { kind: "column", name: element };
  }
  if (element && typeof element === "object") {
    if ("column" in element) {
      requireString(element.column, "index column element column");
      const order = indexColumnOrderToIr(element.order);
      const opclass = indexElementFacet(element.opclass, "index column opclass");
      const collation = indexElementFacet(element.collation, "index column collation");
      return compact({
        kind: "column",
        name: element.column,
        order: order === "desc" ? order : void 0,
        opclass,
        collation
      });
    }
    if ("expr" in element) {
      indexColumnOrderToIr(element.order);
      const expr = resolveImmutableExpr(
        element.expr,
        "index expression element"
      );
      if (!expr) {
        throw structuredError("OP_INVALID", "index expr element needs { expr }");
      }
      return { kind: "expr", expr };
    }
  }
  throw structuredError("OP_INVALID", "index element must be a column name, { column }, or { expr }");
}
function indexColumnOrderToIr(order) {
  if (order === void 0 || order === "asc") {
    return void 0;
  }
  if (order === "desc") {
    return "desc";
  }
  throw structuredError("OP_INVALID", 'index column order must be "asc" or "desc"');
}
function recordAddExclusion(table2, name, args) {
  emitAddExclusion({
    table: table2,
    constraint: exclusionConstraintFromSpec({ ...args, name }),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordDropConstraint(table2, name, args) {
  emitDropConstraint({
    table: table2,
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function recordCreateIndex(table2, name, args) {
  if (!Array.isArray(args.on)) {
    throw structuredError("OP_INVALID", ".index(name).add needs { on: IndexElementArg[] }");
  }
  emitCreateIndex({
    table: table2,
    columns: args.on.map(indexElementToIr),
    name,
    unique: args.unique,
    using: args.using,
    where: resolveImmutableExpr(args.where, "partial index predicate"),
    include: indexIncludeToIr(args.include),
    with: indexWithToIr(args.with),
    only: requireOptionalBoolean(args.only, "index only"),
    nullsNotDistinct: requireOptionalBoolean(args.nullsNotDistinct, "index nullsNotDistinct"),
    schema: args.schema,
    existenceGuard: ifNotExistsGuard(args.ifNotExists)
  });
}
function recordDropIndex(table2, name, args) {
  emitDropIndex({
    name,
    table: table2,
    concurrently: args.concurrently,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists)
  });
}
function normalizeInsertRows(rows, what) {
  if (rows === void 0) throw structuredError("OP_INVALID", `${what}: rows is required`);
  const arr = Array.isArray(rows) ? rows : [rows];
  const columns = arr.length > 0 ? Object.keys(arr[0]) : [];
  const firstKeySet = new Set(columns);
  const normalized = arr.map((r) => {
    const keys = Object.keys(r);
    const values = {};
    for (const key of keys) setOwn(values, key, toIrValue(r[key]));
    return { keys, values };
  });
  for (let i = 0; i < normalized.length; i++) {
    const keys = normalized[i].keys;
    const sameShape = keys.length === columns.length && keys.every((key) => firstKeySet.has(key));
    if (!sameShape) {
      throw structuredError(
        "OP_INVALID",
        `${what}: row ${i} has keys [${keys.join(", ")}], expected [${columns.join(", ")}]; ragged insert rows are not allowed`
      );
    }
  }
  const positional = normalized.map(({ values }) => columns.map((col) => values[col]));
  return { columns, rows: positional };
}
function recordInsert(table2, args) {
  const normalized = normalizeInsertRows(args.rows, "insert({ rows })");
  emitInsert({
    table: table2,
    columns: normalized.columns,
    rows: normalized.rows,
    onConflict: normalizeOnConflict(args.onConflict),
    schema: args.schema
  });
}
function normalizeOnConflict(oc) {
  if (oc === void 0 || oc === null) return void 0;
  if (oc.doUpdate === void 0) return { columns: oc.columns };
  const doUpdate = {};
  for (const col of Object.keys(oc.doUpdate)) {
    setOwn(doUpdate, col, toIrValue(oc.doUpdate[col]));
  }
  return { columns: oc.columns, doUpdate };
}
function recordUpdate(table2, args) {
  emitUpdate({
    table: table2,
    set: resolveSet(args.set),
    where: resolveExpr(args.where),
    schema: args.schema
  });
}
function recordDel(table2, args) {
  if (args.where === void 0 || args.where === null) {
    throw structuredError("OP_INVALID", "delete({ where }): where is mandatory (no unfiltered delete)");
  }
  emitDelete({
    table: table2,
    where: resolveExpr(args.where),
    limit: args.limit,
    schema: args.schema
  });
}
var DEFAULT_BACKFILL_BATCH = 1e3;
function resolveCursorStability(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw structuredError(
      "OP_INVALID",
      'backfill({ cursorStability }) must be { mode: "guardUpdates" } or { mode: "externalInvariant", name: string }'
    );
  }
  const stability = value;
  if (stability.mode === "guardUpdates") {
    const keys = Object.keys(stability);
    if (keys.length !== 1 || keys[0] !== "mode") {
      throw structuredError(
        "OP_INVALID",
        'backfill({ cursorStability: { mode: "guardUpdates" } }) accepts only the mode field'
      );
    }
    return { mode: "guardUpdates" };
  }
  if (stability.mode === "externalInvariant") {
    const keys = Object.keys(stability).sort();
    if (keys.length !== 2 || keys[0] !== "mode" || keys[1] !== "name") {
      throw structuredError(
        "OP_INVALID",
        'backfill({ cursorStability: { mode: "externalInvariant", name } }) accepts exactly mode and name'
      );
    }
    requireNonEmptyString(stability.name, "backfill({ cursorStability.name })");
    return { mode: "externalInvariant", name: stability.name };
  }
  throw structuredError(
    "OP_INVALID",
    'backfill({ cursorStability.mode }) must be "guardUpdates" or "externalInvariant"'
  );
}
function recordBackfill(table2, args) {
  if (Object.prototype.hasOwnProperty.call(args, "cursorColumn")) {
    throw structuredError(
      "OP_INVALID",
      'backfill({ cursorColumn }) was removed; use cursorColumns: ["column"]'
    );
  }
  if (args.set === void 0) throw structuredError("OP_INVALID", "backfill({ set }): set is required");
  requireOrderedColumns(args.cursorColumns, "backfill({ cursorColumns })");
  emitBackfill({
    table: table2,
    cursorColumns: [...args.cursorColumns],
    cursorStability: resolveCursorStability(args.cursorStability),
    batchSize: args.batchSize !== void 0 ? args.batchSize : DEFAULT_BACKFILL_BATCH,
    set: resolveBackfillSet(args.set),
    filter: resolveExpr(args.where),
    name: args.name || `backfill_${table2}`,
    schema: args.schema
  });
}
function normalizeTableRef(input, what) {
  if (typeof input === "string") return { name: input };
  if (!input || typeof input !== "object") {
    throw structuredError("OP_INVALID", `${what} must be a table name string or { name, schema?, alias? }`);
  }
  requireString(input.name, `${what}.name`);
  if (input.schema !== void 0 && input.schema !== null) requireString(input.schema, `${what}.schema`);
  if (input.alias !== void 0 && input.alias !== null) requireString(input.alias, `${what}.alias`);
  return compact({ name: input.name, schema: input.schema ?? void 0, alias: input.alias ?? void 0 });
}
function viewExpr(slot) {
  return resolveExpr(slot);
}
function normalizeSelectItem(item) {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChainImpl) {
    return { kind: "expr", expr: viewExpr(item) };
  }
  if (item && typeof item === "object") {
    const node = item;
    if (node.node !== void 0) return { kind: "expr", expr: viewExpr(node) };
    if (node.kind === "colRef") {
      requireString(node.name, "select item colRef.name");
      if (node.table !== void 0 && node.table !== null) requireString(node.table, "select item colRef.table");
      if (node.alias !== void 0 && node.alias !== null) requireString(node.alias, "select item colRef.alias");
      return compact({
        kind: "colRef",
        table: node.table ?? void 0,
        name: node.name,
        alias: node.alias ?? void 0
      });
    }
    if (node.kind === "expr") {
      if (node.alias !== void 0 && node.alias !== null) requireString(node.alias, "select item expr.alias");
      return compact({
        kind: "expr",
        expr: viewExpr(node.expr),
        alias: node.alias ?? void 0
      });
    }
  }
  throw structuredError("OP_INVALID", "select item must be a column name, expression, or SelectItem object");
}
function normalizeOrderDir(dir, what) {
  if (dir === void 0 || dir === null) return void 0;
  if (dir === "asc" || dir === "desc") return dir;
  throw structuredError("OP_INVALID", `${what}.dir must be asc or desc`);
}
function normalizeOrderItem(item) {
  if (typeof item === "string") return { kind: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChainImpl) {
    return { kind: "expr", expr: viewExpr(item) };
  }
  if (item && typeof item === "object") {
    const node = item;
    if (node.node !== void 0) return { kind: "expr", expr: viewExpr(node) };
    if (node.kind === "colRef") {
      requireString(node.name, "order item colRef.name");
      if (node.table !== void 0 && node.table !== null) requireString(node.table, "order item colRef.table");
      return compact({
        kind: "colRef",
        table: node.table ?? void 0,
        name: node.name,
        dir: normalizeOrderDir(node.dir, "order item colRef")
      });
    }
    if (node.kind === "expr") {
      return compact({
        kind: "expr",
        expr: viewExpr(node.expr),
        dir: normalizeOrderDir(node.dir, "order item expr")
      });
    }
  }
  throw structuredError("OP_INVALID", "orderBy item must be a column name, expression, or OrderItem object");
}
function normalizeGroupByItem(item) {
  if (typeof item === "string") return { node: "colRef", name: item };
  if (typeof item === "function" || item instanceof ExprChainImpl) {
    return viewExpr(item);
  }
  if (item && typeof item === "object" && item.node !== void 0) {
    return viewExpr(item);
  }
  throw structuredError("OP_INVALID", "groupBy item must be a column name or expression");
}
function viewQueryBuilder() {
  const state = {
    projection: [],
    joins: [],
    groupBy: []
  };
  let builder;
  builder = {
    from(table2) {
      state.from = normalizeTableRef(table2, "view query from(table)");
      return builder;
    },
    select(items) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query select(items): items must be an array");
      }
      state.projection = items.map(normalizeSelectItem);
      return builder;
    },
    join(kind, table2, on) {
      if (kind !== "inner" && kind !== "left") {
        throw structuredError("OP_INVALID", "view query join(kind): kind must be inner or left");
      }
      state.joins.push({
        kind,
        table: normalizeTableRef(table2, "view query join(table)"),
        on: viewExpr(on)
      });
      return builder;
    },
    innerJoin(table2, on) {
      return builder.join("inner", table2, on);
    },
    leftJoin(table2, on) {
      return builder.join("left", table2, on);
    },
    where(expr) {
      state.where = viewExpr(expr);
      return builder;
    },
    groupBy(items) {
      if (!Array.isArray(items)) {
        throw structuredError("OP_INVALID", "view query groupBy(items): items must be an array");
      }
      state.groupBy = items.map(normalizeGroupByItem);
      return builder;
    },
    having(expr) {
      state.having = viewExpr(expr);
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
      if (state.from === void 0) {
        throw structuredError("OP_INVALID", "view query must call q.from(table)");
      }
      return compact({
        from: state.from,
        projection: state.projection,
        joins: state.joins.length ? state.joins : void 0,
        where: state.where,
        groupBy: state.groupBy.length ? state.groupBy : void 0,
        having: state.having,
        orderBy: state.orderBy,
        limit: state.limit
      });
    }
  };
  return builder;
}
function isSelectAstBuilder(x) {
  return Boolean(x && typeof x === "object" && typeof x.__selectAst === "function");
}
function isSelectAst(x) {
  return Boolean(x && typeof x === "object" && x.from !== void 0);
}
function isRawViewQueryInput(x) {
  return Boolean(x && typeof x === "object" && Object.prototype.hasOwnProperty.call(x, "raw"));
}
function resolveSelectAst(as) {
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
function recordCreateView(name, args) {
  if (!args || args.as === void 0) {
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
      materialized: args.materialized
    });
    return;
  }
  emitCreateView({
    name,
    schema: args.schema,
    columns: args.columns,
    query: { kind: "structured", select: resolveSelectAst(args.as) },
    replace: args.replace,
    materialized: args.materialized
  });
}
function recordDropView(name, args) {
  emitDropView({
    name,
    schema: args.schema,
    existenceGuard: ifExistsGuard(args.ifExists),
    materialized: args.materialized
  });
}
var TRIGGER_RAISE_LEVELS = ["abort", "fail", "ignore", "rollback"];
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
          { level: args.level }
        );
      }
      requireString(args.message, "b.raise({ message })");
      if (args.errcode !== void 0) requireString(args.errcode, "b.raise({ errcode })");
      return compact({
        stmt: "raise",
        level: args.level,
        message: args.message,
        errcode: args.errcode
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
        schema: args.schema
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
        schema: args.schema
      });
    },
    delete(args) {
      if (!args || typeof args !== "object") {
        throw structuredError("OP_INVALID", "b.delete({ table, where, limit?, schema? }) needs an object");
      }
      requireString(args.table, "b.delete({ table })");
      if (args.where === void 0 || args.where === null) {
        throw structuredError("OP_INVALID", "b.delete({ where }): where is mandatory (no unfiltered delete)");
      }
      return compact({
        stmt: "delete",
        table: args.table,
        where: resolveExpr(args.where),
        limit: args.limit,
        schema: args.schema
      });
    },
    select(expr) {
      return { stmt: "select", expr: resolveExpr(expr) };
    }
  };
}
function resolveTriggerAction(args) {
  const hasExecute = "execute" in args && args.execute !== void 0;
  const hasBody = "body" in args && args.body !== void 0;
  if (hasExecute === hasBody) {
    throw structuredError(
      "OP_INVALID",
      ".trigger(name).create(...) needs exactly one action: { execute: string } or { body: (b) => TriggerStmt[] }"
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
    if (!stmt || typeof stmt !== "object" || typeof stmt.stmt !== "string") {
      throw structuredError("OP_INVALID", "trigger body entries must be statements returned by the trigger body builder");
    }
  }
  return { kind: "body", statements };
}
function pickSchema(perCall, dflt) {
  if (perCall && perCall.schema !== void 0) return perCall.schema;
  return dflt;
}
function pickViewColumns(perCall, dflt) {
  if (perCall && perCall.columns !== void 0) return perCall.columns;
  return dflt;
}
function requireColumnDef(x, where) {
  if (!isColumnDef(x)) {
    throw structuredError("OP_INVALID", `${where} must be a t.* ColumnDef`);
  }
}
function recordAlterPrimaryKey(table2, action, schema2) {
  emitAlterPrimaryKey({ table: table2, action: compact(action), schema: schema2 });
}
function recordSynchronizeIdentity(table2, column, writesQuiesced, schema2) {
  emitSynchronizeIdentity({ table: table2, column, writesQuiesced, schema: schema2 });
}
function comment(target, text) {
  recordComment(target, text);
}
function table(name, opts = {}) {
  return __makeTableHandle(name, opts);
}
function __makeTableHandle(name, opts = {}, checkExprResolver = resolveTableCheckExpr) {
  requireString(name, "table(name, \u2026)");
  const dflt = opts.schema;
  const handle = {
    // The table itself
    create(args) {
      recordCreateTable(name, { ...args, schema: pickSchema(args, dflt) }, checkExprResolver);
      return handle;
    },
    drop(args = {}) {
      recordDropTable(name, {
        ifExists: args.ifExists,
        cascade: args.cascade,
        schema: pickSchema(args, dflt)
      });
      return handle;
    },
    rename(args) {
      requireString(args.to, "table(name).rename({ to })");
      recordRenameTable(name, args.to, {
        ifExists: args.ifExists,
        schema: pickSchema(args, dflt)
      });
      return __makeTableHandle(args.to, opts, checkExprResolver);
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
            schema: pickSchema(args, dflt)
          });
          return handle;
        },
        attach(bound, args = {}) {
          terminateSelector(id);
          recordAttachPartition(name, partitionName, partitionBoundToIr(bound), {
            schema: pickSchema(args, dflt)
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          recordDropPartition(name, partitionName, {
            ifExists: args.ifExists,
            cascade: args.cascade,
            schema: pickSchema(args, dflt)
          });
          return handle;
        },
        detach(args = {}) {
          terminateSelector(id);
          recordDetachPartition(name, partitionName, {
            concurrently: args.concurrently,
            schema: pickSchema(args, dflt)
          });
          return handle;
        }
      };
    },
    primaryKey() {
      return {
        add(args) {
          requirePlainObject(args, ".primaryKey().add(args)");
          requireOrderedColumns(args.columns, ".primaryKey().add({ columns })");
          recordAlterPrimaryKey(name, { kind: "add", columns: args.columns }, dflt);
          return handle;
        },
        replace(args) {
          requirePlainObject(args, ".primaryKey().replace(args)");
          requireOrderedColumns(
            args.expectedColumns,
            ".primaryKey().replace({ expectedColumns })"
          );
          requireOrderedColumns(args.columns, ".primaryKey().replace({ columns })");
          if (args.dropIdentityFrom !== void 0) {
            requireOrderedColumns(
              args.dropIdentityFrom,
              ".primaryKey().replace({ dropIdentityFrom })"
            );
          }
          if (args.expectedColumns.length === args.columns.length && args.expectedColumns.every((column, index) => column === args.columns[index])) {
            throw structuredError(
              "OP_INVALID",
              ".primaryKey().replace({ columns }) must change the ordered primary-key tuple"
            );
          }
          requireDropIdentitySubset(
            args.dropIdentityFrom,
            args.expectedColumns,
            ".primaryKey().replace({ dropIdentityFrom })"
          );
          recordAlterPrimaryKey(
            name,
            {
              kind: "replace",
              expectedColumns: args.expectedColumns,
              columns: args.columns,
              dropIdentityFrom: args.dropIdentityFrom
            },
            dflt
          );
          return handle;
        },
        drop(args) {
          requirePlainObject(args, ".primaryKey().drop(args)");
          requireOrderedColumns(
            args.expectedColumns,
            ".primaryKey().drop({ expectedColumns })"
          );
          if (args.dropIdentityFrom !== void 0) {
            requireOrderedColumns(
              args.dropIdentityFrom,
              ".primaryKey().drop({ dropIdentityFrom })"
            );
          }
          requireDropIdentitySubset(
            args.dropIdentityFrom,
            args.expectedColumns,
            ".primaryKey().drop({ dropIdentityFrom })"
          );
          recordAlterPrimaryKey(
            name,
            {
              kind: "drop",
              expectedColumns: args.expectedColumns,
              dropIdentityFrom: args.dropIdentityFrom
            },
            dflt
          );
          return handle;
        }
      };
    },
    // Columns
    column(col) {
      requireString(col, ".column(name)");
      const id = registerSelector("column", col);
      return {
        add(args) {
          requireColumnDef(args.type, ".column(name).add({ type })");
          terminateSelector(id);
          recordAddColumn(name, col, args.type, {
            ifNotExists: args.ifNotExists,
            schema: pickSchema(args, dflt)
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
        synchronizeIdentity(args) {
          requirePlainObject(args, ".column(name).synchronizeIdentity(args)");
          requireNonEmptyString(
            args.writesQuiesced,
            ".column(name).synchronizeIdentity({ writesQuiesced })"
          );
          if (args.writesQuiesced.trim().length === 0) {
            throw structuredError(
              "OP_INVALID",
              ".column(name).synchronizeIdentity({ writesQuiesced }) must contain non-whitespace text"
            );
          }
          terminateSelector(id);
          recordSynchronizeIdentity(
            name,
            col,
            args.writesQuiesced,
            pickSchema(args, dflt)
          );
          return handle;
        }
      };
    },
    // Constraints. Selector form is THE grammar (one grammar, one
    // spelling). The `addForeignKey`/`addCheck` verb twins are DELETED —
    // `foreignKey(name).add`/`check(name).add` are the SOLE public
    // writers of the `addConstraint` fk/check payload.
    foreignKey(fkName) {
      requireNonEmptyString(fkName, ".foreignKey(name)");
      const id = registerSelector("foreignKey", fkName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddForeignKey(name, fkName, { ...args, schema: pickSchema(args, dflt) });
          return handle;
        }
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
        }
      };
    },
    check(ckName) {
      requireString(ckName, ".check(name)");
      const id = registerSelector("check", ckName);
      return {
        add(args) {
          terminateSelector(id);
          recordAddCheck(name, ckName, { ...args, schema: pickSchema(args, dflt) }, checkExprResolver);
          return handle;
        }
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
        }
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
        validate(args = {}) {
          terminateSelector(id);
          recordValidateConstraint(name, cName, { ifExists: args.ifExists, schema: pickSchema(args, dflt) });
          return handle;
        }
      };
    },
    // Indexes
    index(idxName) {
      requireString(idxName, ".index(name)");
      const id = registerSelector("index", idxName);
      const indexRef = {
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
            schema: pickSchema(args, dflt)
          });
          return handle;
        },
        comment(text, args = {}) {
          terminateSelector(id);
          recordComment({ kind: "index", name: idxName, schema: pickSchema(args, dflt) }, text);
          return handle;
        }
      };
      return indexRef;
    },
    // Table data (no existence guard; schema rides on args)
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
    // Postgres vendor — table-scoped privileged primitives.
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
          if (args.using === void 0) {
            throw structuredError("OP_INVALID", ".policy(name).create({ using }): using is required (the renderer always emits USING)");
          }
          emitCreatePolicy({
            name: policyName,
            table: name,
            schema: pickSchema(args, dflt),
            forCmd: args.for || "all",
            to: args.to,
            using: resolveExpr(args.using),
            withCheck: resolveExpr(args.withCheck)
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          emitDropPolicy({
            name: policyName,
            table: name,
            schema: pickSchema(args, dflt),
            ifExists: args.ifExists
          });
          return handle;
        }
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
            when: resolveExpr(args.when)
          });
          return handle;
        },
        drop(args = {}) {
          terminateSelector(id);
          emitDropTrigger({
            name: triggerName,
            table: name,
            schema: pickSchema(args, dflt),
            ifExists: args.ifExists
          });
          return handle;
        }
      };
    }
  };
  return handle;
}
function view(name, opts = {}) {
  requireString(name, "view(name, \u2026)");
  const dflt = opts.schema;
  const dfltColumns = opts.columns;
  const handle = {
    create(args) {
      recordCreateView(name, {
        ...args,
        schema: pickSchema(args, dflt),
        columns: pickViewColumns(args, dfltColumns)
      });
      return handle;
    },
    drop(args = {}) {
      recordDropView(name, {
        ifExists: args.ifExists,
        materialized: args.materialized,
        schema: pickSchema(args, dflt)
      });
      return handle;
    },
    comment(text, args = {}) {
      recordComment({ kind: "view", name, schema: pickSchema(args, dflt) }, text);
      return handle;
    }
  };
  return handle;
}
var NONDETERMINISM_PATTERNS = [
  { re: /\bDate\s*\.\s*now\s*\(/, name: "Date.now()", steer: "the Date.now symbol (no parens) or now()" },
  { re: /\bMath\s*\.\s*random\s*\(/, name: "Math.random()", steer: "the Math.random symbol (no parens) or uuidV4()" },
  { re: /\bcrypto\s*\.\s*randomUUID\s*\(/, name: "crypto.randomUUID()", steer: "the crypto.randomUUID symbol (no parens) or uuidV4()" },
  { re: /\bnew\s+Date\s*\(/, name: "new Date(...)", steer: "the Date.now symbol (no parens) or now()" }
];
function lintDeterminism(source) {
  if (typeof source !== "string") return [];
  const findings = [];
  for (const { re, name, steer } of NONDETERMINISM_PATTERNS) {
    if (re.test(source)) {
      findings.push({
        code: "NONDETERMINISTIC_OP_ARG",
        accessor: name,
        suggested_fix: `replace ${name} with the DB-evaluated ${steer}`,
        reason: `${name} bakes a build-time value into the artifact; use a structured database expression`
      });
    }
  }
  return findings;
}
function splitPartGrammarLint(delim, n) {
  const fail = (reason) => {
    throw structuredError("EXPR_NOT_PORTABLE", reason, {
      suggested_fix: "pass a non-empty string-literal delimiter and a positive-integer n; to target SQLite too, stay in-envelope (single-ASCII delimiter, 1<=n<=8); out-of-envelope forms are only renderable on dialects with a native renderer such as Postgres/MySQL"
    });
  };
  if (typeof delim !== "string") fail(`.splitPart delimiter must be a string literal; got ${typeof delim}`);
  if (delim.length === 0) fail(".splitPart delimiter must be a non-empty string literal");
  if (typeof n !== "number" || !Number.isInteger(n)) {
    fail(`.splitPart part index n must be a positive integer literal; got ${JSON.stringify(n)}`);
  }
  if (n < 1) fail(`.splitPart part index n must be a positive integer; got ${n}`);
}

export { __begin, __drain, __pgDomain, __pgSequence, byteValue, cCase, check, comment, concatWs, countStar, createFunction, currentSetting, currentUser, decimal, dialect, domain, dropFunction, dropOwnedBy, enumType, extension, genRandomUuid, grant, ids, int64, interval, lintDeterminism, lit, maxValue, minValue, nextval, now, opProducerRegistry, opProducers, perRow, raw, revoke, role, schema, sequence, t, table, uuidV4, uuidV7, view };
//# sourceMappingURL=embedded-recorder.js.map
//# sourceMappingURL=embedded-recorder.js.map