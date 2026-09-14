import type { FieldDef, IdValue } from "./types.js";

export function isIdValue(value: unknown): value is IdValue {
  return typeof value === "string" || typeof value === "bigint"
    || (typeof value === "number" && Number.isFinite(value));
}

/** Numeric decoder representations share a key; text identities stay distinct. */
export function identityKey(value: IdValue): string {
  return `${typeof value === "string" ? "text" : "number"}:${value}`;
}

export function isIdentityForField(value: unknown, field: FieldDef | undefined): value is IdValue {
  switch (field?.type) {
    case "bigInt":
      return typeof value === "bigint" || (typeof value === "number" && Number.isSafeInteger(value));
    case "int": case "integer":
      return typeof value === "number" && Number.isSafeInteger(value);
    case "number": case "float":
      return typeof value === "number" && Number.isFinite(value);
    default:
      return typeof value === "string";
  }
}
