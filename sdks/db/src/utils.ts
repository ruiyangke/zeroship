/**
 * Utilities for @zeroship/db.
 *
 * No field mapping — Postgres native names used everywhere (id, created_at, updated_at).
 * Contains aggregate pipeline translation (MongoDB → native format).
 */
import { PlainObject } from "./types.js";

// ---------------------------------------------------------------------------
// Pass-through (no mapping — Postgres native names)
// ---------------------------------------------------------------------------

/** @internal Map result: created_at→createdAt, updated_at→updatedAt. No _id mapping. */
export function mapResultDoc(doc: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(doc)) {
    if (key === "created_at") result["createdAt"] = val;
    else if (key === "updated_at") result["updatedAt"] = val;
    else result[key] = val;
  }
  return result;
}

/** @internal Map filter: createdAt→created_at, updatedAt→updated_at. Recurses into $and/$or/$not. */
export function mapFilterOutbound(filter: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(filter)) {
    if (key === "createdAt") result["created_at"] = val;
    else if (key === "updatedAt") result["updated_at"] = val;
    else if (key === "$and" || key === "$or") result[key] = (val as PlainObject[]).map(mapFilterOutbound);
    else if (key === "$not") result[key] = mapFilterOutbound(val as PlainObject);
    else result[key] = val;
  }
  return result;
}

/** @internal Map a field name from user-facing to DB column name. */
function mapFieldName(field: string): string {
  if (field === "createdAt") return "created_at";
  if (field === "updatedAt") return "updated_at";
  return field;
}

/** @internal Pass-through: no update mapping needed. */
export function mapUpdateOutbound(update: PlainObject): PlainObject {
  // Still need to transform Mongoose top-level operators to per-field:
  // { $inc: { views: 1 } } → { views: { $inc: 1 } }
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      // $set fields become plain field:value, with createdAt/updatedAt mapped
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[mapFieldName(field)] = fieldVal;
      }
    } else if (key.startsWith("$") && typeof val === "object" && val !== null) {
      // $inc, $dec, $mul, $push, $pull, $addToSet — per-field operators
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[mapFieldName(field)] = { [key]: fieldVal };
      }
    } else {
      result[mapFieldName(key)] = val;
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// Aggregate pipeline translation (MongoDB syntax → native format)
// ---------------------------------------------------------------------------

/** @internal Strip $ prefix from field references */
function stripDollar(val: string): string {
  return val.startsWith("$") ? val.slice(1) : val;
}

/** @internal Translate an accumulator expression */
function translateAccumulator(acc: unknown): unknown {
  if (typeof acc !== "object" || acc === null) return acc;
  const obj = acc as PlainObject;

  if ("$sum" in obj && obj.$sum === 1) return { $count: true };
  if ("$sum" in obj && typeof obj.$sum === "string") return { $sum: stripDollar(obj.$sum as string) };
  if ("$avg" in obj && typeof obj.$avg === "string") return { $avg: stripDollar(obj.$avg as string) };
  if ("$min" in obj && typeof obj.$min === "string") return { $min: stripDollar(obj.$min as string) };
  if ("$max" in obj && typeof obj.$max === "string") return { $max: stripDollar(obj.$max as string) };
  if ("$first" in obj && typeof obj.$first === "string") return { $first: stripDollar(obj.$first as string) };

  return acc;
}

/** @internal Translate _id group key to by */
function translateGroupId(id: unknown): { by: string | string[] } {
  if (typeof id === "string") return { by: stripDollar(id) };
  if (typeof id === "object" && id !== null) {
    const fields: string[] = [];
    for (const val of Object.values(id as PlainObject)) {
      if (typeof val === "string") fields.push(stripDollar(val));
    }
    return { by: fields };
  }
  return { by: String(id) };
}

/** @internal Translate a single pipeline stage */
function translateStage(stage: PlainObject): PlainObject {
  if ("$group" in stage) {
    const group = stage.$group as PlainObject;
    const { id, ...rest } = group;
    const { by } = translateGroupId(id);
    const translated: PlainObject = { by };
    for (const [key, val] of Object.entries(rest)) {
      translated[key] = translateAccumulator(val);
    }
    return { $group: translated };
  }
  return stage;
}

/**
 * @internal
 * Translate a MongoDB-style aggregate pipeline to native format.
 * - `$group._id` → `$group.by`
 * - `"$field"` → `"field"` (strip $ prefix)
 * - `{ $sum: 1 }` → `{ $count: true }`
 */
export function translateAggregatePipeline(pipeline: PlainObject[]): PlainObject[] {
  return pipeline.map(translateStage);
}
