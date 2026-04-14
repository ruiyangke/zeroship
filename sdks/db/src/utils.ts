/**
 * Utilities for @zeroship/db.
 *
 * Field name mapping: createdAt↔created_at, updatedAt↔updated_at.
 * Contains aggregate pipeline translation (MongoDB → native format).
 */
import { PlainObject } from "./types.js";

// ---------------------------------------------------------------------------
// Document result mapping (native → user)
// ---------------------------------------------------------------------------

/** @internal Map result: created_at→createdAt, updated_at→updatedAt. Mutates in-place (safe on freshly parsed JSON). */
export function mapResultDoc(doc: PlainObject): PlainObject {
  if ("created_at" in doc) {
    doc["createdAt"] = doc["created_at"];
    delete doc["created_at"];
  }
  if ("updated_at" in doc) {
    doc["updatedAt"] = doc["updated_at"];
    delete doc["updated_at"];
  }
  return doc;
}

// ---------------------------------------------------------------------------
// Filter mapping (user → native)
// ---------------------------------------------------------------------------

/** @internal Map filter: createdAt→created_at, updatedAt→updated_at. Recurses into $and/$or/$not. */
export function mapFilterOutbound(filter: ZeroshipDbFilter, depth = 0): ZeroshipDbFilter {
  if (depth > 20) throw new Error("filter nesting too deep (max 20 levels)");
  // Fast path: if no key needs remapping, return the original reference
  let needsMap = false;
  for (const key in filter) {
    if (key === "createdAt" || key === "updatedAt" || key === "$and" || key === "$or" || key === "$not") {
      needsMap = true;
      break;
    }
  }
  if (!needsMap) return filter;
  const result: ZeroshipDbFilter = {};
  for (const [key, val] of Object.entries(filter)) {
    if (key === "createdAt") result["created_at"] = val;
    else if (key === "updatedAt") result["updated_at"] = val;
    else if (key === "$and" || key === "$or") result[key] = (val as ZeroshipDbFilter[]).map(f => mapFilterOutbound(f, depth + 1));
    else if (key === "$not") result[key] = mapFilterOutbound(val as ZeroshipDbFilter, depth + 1);
    else result[key] = val;
  }
  return result;
}

// ---------------------------------------------------------------------------
// Update mapping (user → native)
// ---------------------------------------------------------------------------

/** @internal Map a field name from user-facing to DB column name. */
function mapFieldName(field: string): string {
  if (field === "createdAt") return "created_at";
  if (field === "updatedAt") return "updated_at";
  return field;
}

/** @internal Translate Mongoose top-level operators to per-field native format. */
export function mapUpdateOutbound(update: PlainObject): ZeroshipDbUpdate {
  const result: ZeroshipDbUpdate = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[mapFieldName(field)] = fieldVal as ZeroshipDbUpdateValue;
      }
    } else if (key.startsWith("$") && typeof val === "object" && val !== null) {
      for (const [field, fieldVal] of Object.entries(val as PlainObject)) {
        result[mapFieldName(field)] = { [key]: fieldVal } as ZeroshipDbUpdateValue;
      }
    } else {
      result[mapFieldName(key)] = val as ZeroshipDbUpdateValue;
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

/** Aggregate expression — object, string ref, or scalar. */
type AggregateExpr = PlainObject | string | number | boolean | null;

/** @internal Translate an accumulator expression */
function translateAccumulator(acc: AggregateExpr): AggregateExpr {
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
function translateGroupId(id: AggregateExpr): { by: string | string[] } {
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
  if ("$match" in stage) {
    return { $match: mapFilterOutbound(stage.$match as ZeroshipDbFilter) };
  }
  if ("$group" in stage) {
    const group = stage.$group as PlainObject;
    const groupKey = (group._id ?? group.id) as AggregateExpr;
    const { _id: _discardId, id: _discardId2, ...rest } = group;
    const { by } = translateGroupId(groupKey);
    const translated: PlainObject = { by };
    for (const [key, val] of Object.entries(rest)) {
      translated[key] = translateAccumulator(val as AggregateExpr);
    }
    return { $group: translated };
  }
  if ("$having" in stage) {
    return { $having: mapFilterOutbound(stage.$having as ZeroshipDbFilter) };
  }
  if ("$sort" in stage) {
    const sort = stage.$sort as PlainObject;
    const mapped: PlainObject = {};
    for (const [key, val] of Object.entries(sort)) {
      mapped[mapFieldName(key)] = val;
    }
    return { $sort: mapped };
  }
  return stage;
}

/**
 * @internal
 * Translate a MongoDB-style aggregate pipeline to native format.
 */
export function translateAggregatePipeline(pipeline: PlainObject[]): PlainObject[] {
  return pipeline.map(translateStage);
}
