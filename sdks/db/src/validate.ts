/**
 * Document validation for @appbase/db.
 * Validates documents and partial update objects against a NormalizedSchema,
 * collecting all field errors before throwing a single ValidationError.
 */
import { NormalizedSchema } from "./schema.js";
import { FieldDef, PlainObject } from "./types.js";
import { ValidationError, FieldError } from "./errors.js";

type Doc = PlainObject;

/**
 * Validates a single field value against its FieldDef.
 * Appends a FieldError to `errors` if the value is invalid; otherwise returns
 * without side effects.
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
  } else if (type === "array") {
    if (!Array.isArray(value)) {
      errors[key] = { path: key, message: `${key} must be an array` };
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
    !enumVals.includes(value)
  ) {
    errors[key] = {
      path: key,
      message: `${key} must be one of: ${enumVals.join(", ")}`,
    };
    return;
  }
}

/**
 * Validates a full document against the schema.
 * Applies default values for missing optional fields, throws ValidationError
 * if any required fields are absent or any field value is invalid.
 * Returns the (possibly default-filled) document on success.
 */
export function validateDoc(doc: Doc, schema: NormalizedSchema): Doc {
  const errors: Record<string, FieldError> = {};
  const result: Doc = { ...doc };

  for (const [key, def] of Object.entries(schema)) {
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
 * Validates a partial document (e.g. an update's $set payload) against the schema.
 * Does not require required fields or apply defaults — only validates the fields
 * that are present. Throws ValidationError if any present field is invalid.
 */
export function validatePartial(doc: Doc, schema: NormalizedSchema): Doc {
  const errors: Record<string, FieldError> = {};
  const result: Doc = { ...doc };

  for (const [key, value] of Object.entries(result)) {
    const def = schema[key];
    if (!def) continue;
    if (value === undefined || value === null) continue;
    checkField(key, value, def, errors);
  }

  if (Object.keys(errors).length > 0) {
    throw new ValidationError(errors);
  }

  return result;
}
