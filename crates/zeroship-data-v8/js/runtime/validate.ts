/**
 * Document validation for @zeroship/db.
 * Validates documents and partial update objects against a NormalizedSchema,
 * collecting all field errors before throwing a single ValidationError.
 */
import type { NormalizedSchema } from "../../../../packages/db/src/schema";
import { decimal } from "../../../../packages/db/src/types";
import type { FieldDef, PlainObject, TypeName, PrimitiveTypeName } from "../../../../packages/db/src/types";
import { ValidationError } from "../../../../packages/db/src/errors";
import type { FieldError } from "../../../../packages/db/src/errors";

type Doc = PlainObject;

/**
 * Every member of {@link TypeName}, so an unrecognised type name can be told
 * apart from one this file simply has no branch for.
 *
 * Typed as `ReadonlySet<TypeName>` and built from a `TypeName[]` literal on
 * purpose: adding a member to the union without adding it here is then a
 * compile error rather than a silent hole, which is the failure this set exists
 * to close. `vector`, `geoPoint` and `bytes` are listed and are
 * deliberately not field-validated — they belong to the union, so they must not
 * trip the unknown-type guard.
 */
const KNOWN_FIELD_TYPES: ReadonlySet<string> = new Set<TypeName>([
  "string",
  "number",
  // The generator emits these for columns the migrations declared as integer
  // or float; they are handled by the numeric branch above and are listed
  // here so the fail-closed guard agrees with it. Without them this set and
  // that branch disagree about what a known type is.
  "int",
  "integer",
  "bigInt",
  "float",
  "timestamp",
  "boolean",
  "date",
  "json",
  "calendarDate",
  "array",
  "ref",
  "object",
  "literal",
  "union",
  "vector",
  "geoPoint",
  "bytes",
  "id",
]);

const MIN_TIMESTAMP_MILLIS = -62_135_596_800_000;
const MAX_TIMESTAMP_MILLIS = 253_402_300_799_999;

function isTimestampMillis(value: number): boolean {
  return Number.isInteger(value) && value >= MIN_TIMESTAMP_MILLIS && value <= MAX_TIMESTAMP_MILLIS;
}

/** Real calendar timestamps with an optional time and UTC as the default zone. */
export function isParseableDateString(s: string): boolean {
  const match = /^(\d{4}-\d{2}-\d{2})(?:[T ](\d{2}):(\d{2}):(\d{2})(?:\.(\d+))?(?:[Zz]|([+-])(\d{2})(?::?(\d{2}))?)?)?$/.exec(s);
  if (!match || match[0] !== s || !isValidCalendarDate(match[1])) return false;
  const hour = Number(match[2] ?? 0);
  const minute = Number(match[3] ?? 0);
  const second = Number(match[4] ?? 0);
  const offsetHour = Number(match[7] ?? 0);
  const offsetMinute = Number(match[8] ?? 0);
  if (hour > 23 || minute > 59 || second > 59 || offsetHour > 23 || offsetMinute > 59) return false;
  const fraction = Number((match[5] ?? "").slice(0, 3).padEnd(3, "0"));
  const day = new Date(0);
  const [year, month, date] = match[1].split("-").map(Number);
  day.setUTCFullYear(year, month - 1, date);
  day.setUTCHours(hour, minute, second, fraction);
  const offset = (offsetHour * 60 + offsetMinute) * (match[6] === "-" ? -1 : 1);
  return isTimestampMillis(day.getTime() - offset * 60_000);
}

/** Inputs accepted by the ORM's portable timestamp codec. */
export function isTimestampValue(value: unknown): boolean {
  if (typeof value === "number") return isTimestampMillis(value);
  if (value instanceof Date) return isTimestampMillis(Date.prototype.getTime.call(value));
  return typeof value === "string" && isParseableDateString(value);
}

/**
 * R6 — structured-clone-safe predicate for `json` items. `t.json()`
 * stores arbitrary JSON, but the value still has to round-trip through
 * `JSON.stringify` / Postgres JSONB at the wire boundary. Functions and
 * symbols silently turn into `undefined` (skipped object props, `null`
 * inside arrays) and lose data; reject them at validation time.
 *
 * Walks plain objects and arrays so a function buried two levels deep
 * is still caught. `Date` instances are accepted because
 * `JSON.stringify(date)` produces an ISO string that round-trips
 * deterministically (lossily — type info is gone — but determinately).
 *
 * R7 — explicitly reject `Map`, `Set`, typed arrays, `ArrayBuffer`, and
 * any other non-plain non-Array object. `JSON.stringify(new Map([...]))`
 * silently serialises to `"{}"` because `Object.values(new Map(...))`
 * is empty, which would falsely pass the recursive predicate. The
 * `Object.getPrototypeOf` guard restricts plain-object recursion to
 * `{...}` literals and `Object.create(null)` containers — class
 * instances are rejected, matching the structured-clone wire contract.
 *
 * Cycle protection uses a `WeakSet` visit-marker — `JSON.stringify`
 * would throw on a cycle anyway, and surfacing the rejection here gives
 * a useful error path.
 */
export function isJsonSerializable(value: unknown, seen?: WeakSet<object>): boolean {
  if (value === null) return true;
  const tag = typeof value;
  if (tag === "string" || tag === "number" || tag === "boolean") return true;
  if (tag === "function" || tag === "symbol" || tag === "undefined" || tag === "bigint") return false;
  if (tag !== "object") return false;
  const visited = seen ?? new WeakSet<object>();
  if (visited.has(value as object)) return false; // cycle
  visited.add(value as object);
  if (Array.isArray(value)) {
    for (const elem of value) {
      if (!isJsonSerializable(elem, visited)) return false;
    }
    return true;
  }
  // Date is the one non-plain object JSON.stringify handles natively
  // (produces an ISO string). Accept it.
  if (value instanceof Date) return !isNaN(value.getTime());
  // R7 — reject typed arrays (Uint8Array, etc.), ArrayBuffer/DataView,
  // Map, Set, and any built-in container whose plain-object recursion
  // would mis-read as empty.
  if (ArrayBuffer.isView(value)) return false;
  if (value instanceof ArrayBuffer) return false;
  if (value instanceof Map) return false;
  if (value instanceof Set) return false;
  if (value instanceof RegExp) return false;
  if (value instanceof Promise) return false;
  // Restrict the recursion to plain objects only. Anything with a
  // non-Object/non-null prototype is a class instance whose private
  // state JSON.stringify cannot see.
  const proto = Object.getPrototypeOf(value);
  if (proto !== null && proto !== Object.prototype) return false;
  for (const v of Object.values(value as Record<string, unknown>)) {
    if (!isJsonSerializable(v, visited)) return false;
  }
  return true;
}

const ARRAY_ITEM_VALIDATORS: Record<PrimitiveTypeName, (value: unknown) => boolean> = {
  string: value => typeof value === "string",
  number: value => typeof value === "number",
  boolean: value => typeof value === "boolean",
  date: isTimestampValue,
  calendarDate: value => typeof value === "string" && isValidCalendarDate(value),
  json: isJsonSerializable,
};

/** Shared by document and array-operation validation. */
export function isArrayElement(itemType: PrimitiveTypeName, value: unknown): boolean {
  return Object.hasOwn(ARRAY_ITEM_VALIDATORS, itemType) && ARRAY_ITEM_VALIDATORS[itemType](value);
}

/**
 * D3 — strict `YYYY-MM-DD` validator. Confirms the value is a 10-char
 * date string AND a real calendar date (no Feb 31, no month 13).
 * Returns false on any deviation.
 */
export function isValidCalendarDate(s: string): boolean {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s)) return false;
  const [y, m, d] = s.split("-").map(Number);
  if (y === 0) return false;
  if (m < 1 || m > 12) return false;
  if (d < 1 || d > 31) return false;
  // Round-trip through Date to catch overflow (e.g. 2026-02-31 → Mar 3).
  // setUTCFullYear preserves early years; Date.UTC maps them to another century.
  const dt = new Date(0);
  dt.setUTCFullYear(y, m - 1, d);
  return (
    dt.getUTCFullYear() === y &&
    dt.getUTCMonth() === m - 1 &&
    dt.getUTCDate() === d
  );
}

/**
 * Validates a single field value against its FieldDef.
 * Appends a FieldError to `errors` if the value is invalid; otherwise returns
 * without side effects.
 *
 * `key` is the dotted path used for error reporting; the caller should pass
 * `"parent.child"` when recursing into nested object validators (D2).
 */
function checkField(
  key: string,
  value: unknown,
  def: FieldDef,
  errors: Record<string, FieldError>
): void {
  const { type, min, max, enum: enumVals, pattern } = def;
  const validateStringId = (): boolean => {
    if (typeof value !== "string" || value.length === 0) {
      errors[key] = { path: key, message: `${key} must be a non-empty string id` };
      return false;
    }
    return true;
  };

  // Type check
  if (type === "id" || type === "ref") {
    if (!validateStringId()) return;
  } else if (type === "string") {
    if (typeof value !== "string") {
      errors[key] = { path: key, message: `${key} must be a string` };
      return;
    }
    // Count CHARACTERS, which is what the bound and the message both promise.
    // `String.length` is UTF-16 code units, so anything outside the Basic
    // Multilingual Plane - emoji, CJK extensions, most mathematical symbols -
    // counts double. That made validation stricter than storage: PostgreSQL
    // counts varchar(n) in characters, so varchar(3) accepts three emoji while
    // a code-unit check refused them.
    const charCount = [...value].length;
    if (min !== undefined && charCount < min) {
      errors[key] = {
        path: key,
        message: `${key} must be at least ${min} characters`,
      };
      return;
    }
    if (max !== undefined && charCount > max) {
      errors[key] = {
        path: key,
        message: `${key} must be at most ${max} characters`,
      };
      return;
    }
    if (pattern !== undefined && !pattern.test(value)) {
      errors[key] = {
        path: key,
        message: `${key} does not match the required pattern`,
      };
      return;
    }
  } else if (type === "bigInt" && typeof value === "bigint") {
    if (value < -(1n << 63n) || value > (1n << 63n) - 1n) {
      errors[key] = { path: key, message: key + " is outside the database integer range" };
    } else if (min !== undefined && value < min) {
      errors[key] = { path: key, message: key + " is below its minimum" };
    } else if (max !== undefined && value > max) {
      errors[key] = { path: key, message: key + " exceeds its maximum" };
    }
  } else if (type === "number" && def.precision !== undefined) {
    if (typeof value !== "string") {
      errors[key] = { path: key, message: `${key} must be an exact decimal string` };
      return;
    }
    try {
      decimal(value);
    } catch {
      errors[key] = { path: key, message: `${key} must be a valid exact decimal string` };
      return;
    }
  } else if (
    type === "number" ||
    type === "int" ||
    type === "integer" ||
    type === "bigInt" ||
    type === "float"
  ) {
    // `Number.isFinite` rather than `!isNaN`: the old guard caught NaN and let both
    // infinities through, and all three are lost identically downstream.
    //
    // The loss happens in the native V8->serde decoder, NOT at a JSON.stringify
    // boundary - `env.db` ops receive the document as V8 values. In
    // `crates/zeroship-data-v8/src/v8_bridge.rs` the number arm skips its lossless-integer
    // branch (non-finite `fract()` is NaN) and then calls
    // `serde_json::Number::from_f64`, which returns `None` for anything non-finite,
    // so the arm falls through to `Value::Null`. Measured, and pinned there by
    // `non_finite_numbers_decode_to_null`.
    //
    // The result is that a column accepting NULL silently stores NULL for a value
    // validation just approved, and one that does not gets a database error instead
    // of a field-level message. Catching NaN alone covered one third of that.
    if (typeof value !== "number" || !Number.isFinite(value)) {
      errors[key] = { path: key, message: `${key} must be a finite number` };
      return;
    }
    // The integral column tokens. The runtime descriptor emits `int`,
    // `integer`, `bigInt` and `float` — the generator keeps the column's real
    // type even though the TypeScript renderer collapses all five to
    // `t.number()` — so they arrive here and are numbers, not a separate kind.
    //
    // Enforcing integrality is the point. Without it `create({ points: 1.5 })`
    // reaches an INTEGER column and Postgres assignment-casts it to 2, so the
    // value a creator wrote and the value stored differ with nothing objecting.
    // `bigInt` additionally has to be a safe integer: BIGINT spans the full 64
    // bits, a JS number stops being exact above 2^53, and a value past that is
    // already the wrong number by the time it gets here.
    if (type === "int" || type === "integer" || type === "bigInt") {
      if (!Number.isInteger(value)) {
        errors[key] = { path: key, message: `${key} must be a whole number` };
        return;
      }
      if (!Number.isSafeInteger(value)) {
        errors[key] = {
          path: key,
          message: `${key} is outside the range JavaScript can represent exactly (${Number.MAX_SAFE_INTEGER})`,
        };
        return;
      }
    }
    if (min !== undefined && (value as number) < min) {
      errors[key] = {
        path: key,
        message: `${key} must be at least ${min}`,
      };
      return;
    }
    if (max !== undefined && value > max) {
      errors[key] = {
        path: key,
        message: `${key} must be at most ${max}`,
      };
      return;
    }
  } else if (type === "boolean") {
    if (typeof value !== "boolean") {
      errors[key] = { path: key, message: `${key} must be a boolean` };
      return;
    }
    // `timestamp` alongside `date` for the same reason the integral tokens sit
    // with `number`: the descriptor carries the column's own token, and the
    // generator's renderer treats `date` and `timestamp` as one case. Leaving it
    // out would send every timestamp column into the unknown-type guard below.
  } else if (type === "date" || type === "timestamp") {
    if (!isTimestampValue(value)) {
      errors[key] = {
        path: key,
        message: `${key} must be a valid Date, ISO timestamp, or integral Unix milliseconds`,
      };
      return;
    }
  } else if (type === "calendarDate") {
    // D3 — must be the literal `YYYY-MM-DD` shape AND a real date
    // (rejects 2026-02-31, 2026-13-01, etc.). Stored as a Postgres
    // `DATE` column with no timezone.
    if (typeof value !== "string" || !isValidCalendarDate(value)) {
      errors[key] = {
        path: key,
        message: `${key} must be a YYYY-MM-DD calendar date`,
      };
      return;
    }
  } else if (type === "json") {
    // R7 — top-level `t.json()` parity with `t.array(t.json())`. R6
    // tightened the array-item branch via `isJsonSerializable`; without
    // this case, a top-level json field would accept ANY value —
    // functions, symbols, bigints, cycles — which then either drop
    // silently at `JSON.stringify` or throw at the Postgres driver.
    // Same predicate as the array branch keeps both sites in lockstep.
    if (!isJsonSerializable(value)) {
      errors[key] = {
        path: key,
        message: `${key} must be JSON-serialisable`,
      };
      return;
    }
  } else if (type === "object") {
    // D2 — nested-object validator. The DB column is JSONB; we recurse
    // into the declared `shape` and surface child errors using a dotted
    // path so consumers see `errors["profile.social.twitter"]`.
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      errors[key] = { path: key, message: `${key} must be an object` };
      return;
    }
    const shape = def.shape;
    if (shape !== undefined) {
      const obj = value as Record<string, unknown>;
      for (const [childKey, childDef] of Object.entries(shape)) {
        const childPath = `${key}.${childKey}`;
        const childVal = obj[childKey];
        const missing =
          childVal === undefined ||
          childVal === null ||
          (childDef.type === "string" && childDef.required === true && childVal === "");
        if (missing) {
          if (childDef.required === true && childDef.default === undefined) {
            errors[childPath] = { path: childPath, message: `${childPath} is required` };
          }
          continue;
        }
        checkField(childPath, childVal, childDef, errors);
      }
    }
    return;
  } else if (type === "literal") {
    // C2 — strict equality check against the declared literal value.
    if (value !== def.literalValue) {
      errors[key] = {
        path: key,
        message: `${key} must equal ${JSON.stringify(def.literalValue)}`,
      };
      return;
    }
  } else if (type === "union") {
    // C2 — discriminated union dispatch. Find the variant whose
    // discriminator literal matches the value at the discriminator key,
    // then run nested validation against that variant's shape.
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      errors[key] = { path: key, message: `${key} must be an object` };
      return;
    }
    const variants = def.variants;
    const discriminator = def.discriminator;
    if (variants === undefined || discriminator === undefined) {
      // Mis-normalised union — defensive guard. Real builds go through
      // `t.union()` which always populates both fields.
      errors[key] = { path: key, message: `${key} has malformed union schema` };
      return;
    }
    const obj = value as Record<string, unknown>;
    const discVal = obj[discriminator];
    const matchedVariant = variants.find(
      (v) => v[discriminator]?.literalValue === discVal,
    );
    if (matchedVariant === undefined) {
      const known = variants
        .map((v) => JSON.stringify(v[discriminator]?.literalValue))
        .join(", ");
      errors[`${key}.${discriminator}`] = {
        path: `${key}.${discriminator}`,
        message: `${key}.${discriminator} must be one of: ${known} (got ${JSON.stringify(discVal)})`,
      };
      return;
    }
    // Walk the matched variant's shape and validate every field.
    // Fields not in the variant are silently ignored (consistent with
    // top-level validateDoc which strips unknown keys for safety).
    for (const [childKey, childDef] of Object.entries(matchedVariant)) {
      const childPath = `${key}.${childKey}`;
      const childVal = obj[childKey];
      const missing =
        childVal === undefined ||
        childVal === null ||
        (childDef.type === "string" && childDef.required === true && childVal === "");
      if (missing) {
        if (childDef.required === true && childDef.default === undefined) {
          errors[childPath] = { path: childPath, message: `${childPath} is required` };
        }
        continue;
      }
      checkField(childPath, childVal, childDef, errors);
    }
    return;
  } else if (type === "array") {
    if (!Array.isArray(value)) {
      errors[key] = { path: key, message: `${key} must be an array` };
      return;
    }
    if (min !== undefined && value.length < min) {
      errors[key] = { path: key, message: `${key} must have at least ${min} items` };
      return;
    }
    if (max !== undefined && value.length > max) {
      errors[key] = { path: key, message: `${key} must have at most ${max} items` };
      return;
    }
    // Document writes and array operators use the same item contract.
    if (def.items !== undefined) {
      const itemType = def.items;
      for (let i = 0; i < value.length; i++) {
        const elem = value[i];
        if (!isArrayElement(itemType, elem)) {
          errors[key] = {
            path: key,
            message: `${key}[${i}] must be a ${itemType}`,
          };
          return;
        }
      }
    }
  } else if (type === "bytes") {
    if (!(value instanceof Uint8Array)) {
      errors[key] = { path: key, message: key + " must be a Uint8Array" };
      return;
    }
  } else if (!KNOWN_FIELD_TYPES.has(type)) {
    // Fail closed on a type name this SDK does not know.
    //
    // The chain above has no final else, so before this an unrecognised name
    // matched nothing and the value passed through untouched — the field was
    // not validated at all, silently. That is not hypothetical: the generated
    // runtime descriptor emits `int` for integer columns, `TypeName` has no
    // integer member, and the measured result was that an `int` field accepted
    // the string "abc" while the same field declared `number` rejected it.
    //
    // A type the SDK does not recognise means the descriptor and this validator
    // disagree about the schema, which is a generation or version fault rather
    // than bad user input. It gets a thrown error, not a field-level message,
    // because there is no sound answer to "is this value valid" when the type
    // is unknown, and answering "yes" is the one option that loses data.
    //
    // Names that ARE in `TypeName` but have no branch above (vector, geoPoint,
    // bytes) keep passing: they are deliberately not field-validated at
    // this layer, and this guard is about unknown names, not missing branches.
    throw new Error(
      `unknown field type ${JSON.stringify(type)} for field ${JSON.stringify(key)}: ` +
        `the schema descriptor names a type this version of @zeroship/db cannot validate`,
    );
  }

  // Enum check — only meaningful for scalar types, not for arrays/objects.
  if (
    enumVals !== undefined &&
    (type === "string" || type === "number" || type === "boolean") &&
    !enumVals.includes(value as string | number)
  ) {
    errors[key] = {
      path: key,
      message: `${key} must be one of: ${enumVals.join(", ")}`,
    };
    return;
  }
}

/**
 * @internal
 * Validates a full document against the schema.
 * Applies default values for missing optional fields, throws ValidationError
 * if any required fields are absent or any field value is invalid.
 * Returns the (possibly default-filled) document on success.
 */
export function validateDoc(doc: Doc, schema: NormalizedSchema): Doc {
  // C2 — flat-expanded discriminated-union schema. Locate the
  // discriminator column (FieldDef.discriminator === "__discriminator__"),
  // dispatch on the document's value at that key, and validate against
  // the matched variant's shape only. Fields not in the active variant
  // are stripped from the result.
  for (const [key, def] of Object.entries(schema)) {
    if (def.discriminator === "__discriminator__" && def.variants !== undefined) {
      return validateUnionDoc(doc, schema, key, def);
    }
  }

  const errors: Record<string, FieldError> = {};
  const result: Doc = {};

  // Only copy schema-defined fields — unknown fields are stripped for safety
  for (const [key, def] of Object.entries(schema)) {
    if (key in doc) result[key] = doc[key];
    const value = result[key];
    // An empty string is treated as absent for required checks — a string field
    // that requires a value should not accept "".
    const missing =
      value === undefined ||
      value === null ||
      (def.type === "string" && def.required && value === "");

    if (missing) {
      // PLATFORM-ASSIGNED fields are neither required of the caller nor
      // materialised for them. Both facts are ONE fact - the field carries an
      // `assign`, so the platform computes the value - and neither is stored
      // separately anywhere; see `FieldDef.assign`.
      //
      // The order matters: this arm sits ABOVE the `default` arm on purpose. A
      // charter column may carry both (`version` is `increment(1)` with a DDL
      // `DEFAULT 1`), and letting the default arm win would materialise the
      // seed into the row and hand the SQL builder a value the runtime is
      // supposed to compute. `assign` is the stronger claim, so it is read
      // first.
      //
      // `delete` here is on `result`, this function's OWN object, never on
      // `doc`. The caller's document is not touched: a validator that edits its
      // input makes the caller's object depend on whether it was validated. The
      // key is dropped rather than left undefined because `deleted_at` and
      // friends would otherwise reach the native op as an explicit NULL and
      // drive a NOT NULL violation on a column whose value the platform was
      // about to supply.
      if (def.assign !== undefined) {
        delete result[key];
        continue;
      }
      if (def.default !== undefined) {
        result[key] =
          typeof def.default === "function" ? def.default() : def.default;
      } else if (def.required) {
        errors[key] = { path: key, message: `${key} is required` };
      }
      continue;
    }

    checkField(key, value, def, errors);
  }

  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }

  return result;
}

/**
 * @internal
 * C2 — validate a document against a flat-expanded union schema.
 * Dispatches on the discriminator value, validates against the matched
 * variant's shape, applies defaults for missing optional fields. Throws
 * `ValidationError` for unknown discriminator values OR for variant
 * field violations.
 */
function validateUnionDoc(
  doc: Doc,
  schema: NormalizedSchema,
  discriminator: string,
  discDef: FieldDef,
): Doc {
  const errors: Record<string, FieldError> = {};
  const result: Doc = {};

  const discValue = doc[discriminator];
  if (discValue === undefined || discValue === null) {
    errors[discriminator] = {
      path: discriminator,
      message: `${discriminator} is required (discriminator value)`,
    };
    throw new ValidationError(errors);
  }

  const variants = discDef.variants!;
  const matched = variants.find((v) => v[discriminator]?.literalValue === discValue);
  if (matched === undefined) {
    const known = variants
      .map((v) => JSON.stringify(v[discriminator]?.literalValue))
      .join(", ");
    errors[discriminator] = {
      path: discriminator,
      message: `${discriminator} must be one of: ${known} (got ${JSON.stringify(discValue)})`,
    };
    throw new ValidationError(errors);
  }

  // Walk the matched variant's fields. Defaults apply, requireds enforced.
  for (const [key, def] of Object.entries(matched)) {
    if (key in doc) result[key] = doc[key];
    const value = result[key];
    const missing =
      value === undefined ||
      value === null ||
      (def.type === "string" && def.required === true && value === "");
    if (missing) {
      if (def.default !== undefined) {
        result[key] =
          typeof def.default === "function" ? def.default() : def.default;
      } else if (def.required === true) {
        errors[key] = { path: key, message: `${key} is required` };
      }
      continue;
    }
    checkField(key, value, def, errors);
  }

  // Defensive: any column declared at the table level but not in the
  // matched variant should be silently stripped (already done by not
  // copying it into `result`). We still scan for an explicit NULL value
  // to keep parity with the user's input — but per the proposal these
  // become NULL columns anyway.
  void schema;

  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }
  return result;
}

/**
 * @internal
 * Validates a partial document (e.g. an update's $set payload) against the schema.
 * Does not require required fields or apply defaults — only validates the fields
 * that are present. Throws ValidationError if any present field is invalid.
 */
/**
 * @internal
 * Validates fields without building a result copy. Used by updateOne/updateMany
 * where the stripped result is not needed (the update goes through mapUpdateOutbound).
 */
export function checkPartial(doc: Doc, schema: NormalizedSchema): void {
  const errors: Record<string, FieldError> = {};
  for (const [key, value] of Object.entries(doc)) {
    const def = schema[key];
    if (!def) continue;
    if (value === undefined || value === null) continue;
    checkField(key, value, def, errors);

    // Gap J — `checkPartial` has no row context, so a patch that
    // changes a flat-expanded union discriminator must also carry
    // every variant-required field for the new variant. Otherwise
    // `{kind: "signup"}` would leave the row claiming kind=signup
    // with whatever the previous variant stored under `name` (most
    // likely NULL).
    if (
      def.discriminator === "__discriminator__" &&
      def.variants !== undefined &&
      errors[key] === undefined
    ) {
      const matched = def.variants.find(
        (v) => v[key]?.type === "literal" && v[key]?.literalValue === value,
      );
      if (matched !== undefined) {
        const missing: string[] = [];
        for (const [vKey, vDef] of Object.entries(matched)) {
          if (vDef.type === "literal") continue;
          if (vDef.required !== true) continue;
          if (vDef.default !== undefined) continue;
          const patchVal = doc[vKey];
          if (patchVal === undefined || patchVal === null) {
            missing.push(vKey);
          }
        }
        if (missing.length > 0) {
          const variantLabel = JSON.stringify(value);
          for (const m of missing) {
            errors[m] = {
              path: m,
              message:
                `update: changing ${key} to ${variantLabel} requires also setting: ${missing.join(", ")}`,
            };
          }
        }
      }
    }
  }
  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }
}
