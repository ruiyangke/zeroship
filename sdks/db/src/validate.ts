/**
 * Document validation for @zeroship/db.
 * Validates documents and partial update objects against a NormalizedSchema,
 * collecting all field errors before throwing a single ValidationError.
 */
import { NormalizedSchema } from "./schema.js";
import { FieldDef, PlainObject } from "./types.js";
import { ValidationError, FieldError } from "./errors.js";

type Doc = PlainObject;

/**
 * D3 — strict `YYYY-MM-DD` validator. Confirms the value is a 10-char
 * date string AND a real calendar date (no Feb 31, no month 13).
 * Returns false on any deviation.
 */
function isValidCalendarDate(s: string): boolean {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s)) return false;
  const [y, m, d] = s.split("-").map(Number);
  if (m < 1 || m > 12) return false;
  if (d < 1 || d > 31) return false;
  // Round-trip through Date to catch overflow (e.g. 2026-02-31 → Mar 3).
  const dt = new Date(Date.UTC(y, m - 1, d));
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

  // Type check
  if (type === "string") {
    if (typeof value !== "string") {
      errors[key] = { path: key, message: `${key} must be a string` };
      return;
    }
    if (min !== undefined && value.length < min) {
      errors[key] = {
        path: key,
        message: `${key} must be at least ${min} characters`,
      };
      return;
    }
    if (max !== undefined && value.length > max) {
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
  } else if (type === "number") {
    if (typeof value !== "number" || isNaN(value as number)) {
      errors[key] = { path: key, message: `${key} must be a number` };
      return;
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
  } else if (type === "ref") {
    // B2 — refs are stored as integers at the DB level (BIGINT FK). The
    // brand `Id<T>` is purely type-level; at runtime the value is a
    // plain number. Validate as number and accept it.
    if (typeof value !== "number" || isNaN(value as number)) {
      errors[key] = { path: key, message: `${key} must be a numeric id` };
      return;
    }
  } else if (type === "boolean") {
    if (typeof value !== "boolean") {
      errors[key] = { path: key, message: `${key} must be a boolean` };
      return;
    }
  } else if (type === "date") {
    if (!(value instanceof Date) && (typeof value !== "string" || isNaN(Date.parse(value)))) {
      errors[key] = {
        path: key,
        message: `${key} must be a Date or date string`,
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
    // Validate each array element against the declared item type.
    if (def.items !== undefined) {
      const itemType = def.items;
      for (let i = 0; i < value.length; i++) {
        const elem = value[i];
        let ok = true;
        if (itemType === "string") ok = typeof elem === "string";
        else if (itemType === "number") ok = typeof elem === "number";
        else if (itemType === "boolean") ok = typeof elem === "boolean";
        else if (itemType === "date") ok = elem instanceof Date || (typeof elem === "string" && !isNaN(Date.parse(elem)));
        if (!ok) {
          errors[key] = {
            path: key,
            message: `${key}[${i}] must be a ${itemType}`,
          };
          return;
        }
      }
    }
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
  }
  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }
}

