import type { FieldDef } from "./types";

export function primaryKey(schema: Record<string, FieldDef>): string {
  const keys = Object.keys(schema).filter((key) => schema[key].primaryKey === true);
  if (keys.length !== 1) throw new Error("operation requires an unambiguous primary key");
  return keys[0];
}
