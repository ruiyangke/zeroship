/**
 * Field-name mapping utilities for @appbase/db.
 * Converts between the JS-facing camelCase API names (_id, createdAt, updatedAt)
 * and the native snake_case / plain names used by the underlying data layer.
 */
import { PlainObject } from "./types.js";

// ---------------------------------------------------------------------------
// Inbound mapping: native result → user-facing doc
// id → _id, created_at → createdAt, updated_at → updatedAt
// ---------------------------------------------------------------------------

/**
 * Maps a native result document to the user-facing shape.
 * Renames `id`→`_id`, `created_at`→`createdAt`, `updated_at`→`updatedAt`.
 * All other fields are passed through unchanged.
 */
export function mapResultDoc(doc: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(doc)) {
    if (key === "id") {
      result["_id"] = val;
    } else if (key === "created_at") {
      result["createdAt"] = val;
    } else if (key === "updated_at") {
      result["updatedAt"] = val;
    } else {
      result[key] = val;
    }
  }
  return result;
}

// ---------------------------------------------------------------------------
// Outbound mapping: user filter → native filter
// _id → id, createdAt → created_at, updatedAt → updated_at (deep, handles $and/$or/$not)
// ---------------------------------------------------------------------------

/**
 * Maps a user-supplied filter to the native format.
 * Renames `_id`→`id`, `createdAt`→`created_at`, `updatedAt`→`updated_at`.
 * Recurses into `$and`, `$or`, and `$not` operators so field names are
 * translated at every nesting level.
 */
export function mapFilterOutbound(filter: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(filter)) {
    if (key === "_id") {
      result["id"] = val;
    } else if (key === "createdAt") {
      result["created_at"] = val;
    } else if (key === "updatedAt") {
      result["updated_at"] = val;
    } else if (key === "$and" || key === "$or") {
      result[key] = (val as PlainObject[]).map(mapFilterOutbound);
    } else if (key === "$not") {
      result[key] = mapFilterOutbound(val as PlainObject);
    } else {
      result[key] = val;
    }
  }
  return result;
}

/**
 * Maps an update object's field names from user-facing to native format.
 * Handles both `$set`/`$unset` operator objects and bare top-level field maps.
 * Does NOT touch `$push`/`$addToSet`/`$inc`/`$dec`/`$mul` — those are handled
 * separately by the collection layer.
 */
export function mapUpdateOutbound(update: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(update)) {
    if ((key === "$set" || key === "$unset") && typeof val === "object" && val !== null) {
      result[key] = mapUpdateFields(val as PlainObject);
    } else if (key.startsWith("$")) {
      // Other operators ($push, $addToSet, $inc, etc.) — pass through unchanged.
      result[key] = val;
    } else {
      // Bare field — apply name mapping.
      result[mapFieldName(key)] = val;
    }
  }
  return result;
}

/** Maps a single user-facing field name to its native counterpart. */
function mapFieldName(key: string): string {
  if (key === "_id") return "id";
  if (key === "createdAt") return "created_at";
  if (key === "updatedAt") return "updated_at";
  return key;
}

/** Applies mapFieldName to every key in a plain object. */
function mapUpdateFields(fields: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [k, v] of Object.entries(fields)) {
    result[mapFieldName(k)] = v;
  }
  return result;
}

// ---------------------------------------------------------------------------
// Aggregate pipeline translation: MongoDB → native format
// ---------------------------------------------------------------------------

function stripDollar(val: string): string {
  return val.startsWith("$") ? val.slice(1) : val;
}

/** Translates a single MongoDB accumulator expression to the native equivalent. */
function translateAccumulator(
  acc: unknown
): unknown {
  if (typeof acc !== "object" || acc === null) return acc;
  const obj = acc as PlainObject;

  // { $sum: 1 } → { $count: true }
  if ("$sum" in obj && obj["$sum"] === 1) {
    return { $count: true };
  }

  // { $sum: "$field" } → { $sum: "field" }
  if ("$sum" in obj && typeof obj["$sum"] === "string") {
    return { $sum: stripDollar(obj["$sum"] as string) };
  }

  // { $avg: "$field" } → { $avg: "field" }
  if ("$avg" in obj && typeof obj["$avg"] === "string") {
    return { $avg: stripDollar(obj["$avg"] as string) };
  }

  // { $min: "$field" } → { $min: "field" }
  if ("$min" in obj && typeof obj["$min"] === "string") {
    return { $min: stripDollar(obj["$min"] as string) };
  }

  // { $max: "$field" } → { $max: "field" }
  if ("$max" in obj && typeof obj["$max"] === "string") {
    return { $max: stripDollar(obj["$max"] as string) };
  }

  // { $first: "$field" } → { $first: "field" }
  if ("$first" in obj && typeof obj["$first"] === "string") {
    return { $first: stripDollar(obj["$first"] as string) };
  }

  return acc;
}

/** Translates a MongoDB $group `_id` value to the native `by` format. */
function translateGroupId(
  id: unknown
): { by: string | string[] } {
  // Single field: "$fieldName" → by: "fieldName"
  if (typeof id === "string") {
    return { by: stripDollar(id) };
  }

  // Multi-field: { a: "$x", b: "$y" } → by: ["x", "y"]
  if (typeof id === "object" && id !== null) {
    const obj = id as PlainObject;
    const fields: string[] = [];
    for (const val of Object.values(obj)) {
      if (typeof val === "string") {
        fields.push(stripDollar(val));
      }
    }
    return { by: fields };
  }

  return { by: String(id) };
}

/** Translates a single MongoDB aggregate stage to the native format. */
function translateStage(stage: PlainObject): PlainObject {
  if ("$group" in stage) {
    const group = stage["$group"] as PlainObject;
    const { _id, ...rest } = group;
    const { by } = translateGroupId(_id);

    const translated: PlainObject = { by };
    for (const [accKey, accVal] of Object.entries(rest)) {
      translated[accKey] = translateAccumulator(accVal);
    }
    return { $group: translated };
  }

  if ("$match" in stage) {
    return { $match: mapFilterOutbound(stage["$match"] as PlainObject) };
  }

  if ("$sort" in stage) {
    return stage;
  }

  if ("$limit" in stage || "$skip" in stage || "$project" in stage || "$unwind" in stage) {
    return stage;
  }

  return stage;
}

/**
 * Translates a full MongoDB-style aggregate pipeline to the native format.
 * Each stage is translated individually; unrecognized stages pass through as-is.
 */
export function translateAggregatePipeline(
  pipeline: PlainObject[]
): PlainObject[] {
  return pipeline.map(translateStage);
}
