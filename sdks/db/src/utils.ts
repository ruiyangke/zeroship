type PlainObject = Record<string, unknown>;

// ---------------------------------------------------------------------------
// Inbound mapping: native result → user-facing doc
// id → _id, created_at → createdAt, updated_at → updatedAt
// ---------------------------------------------------------------------------
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
// _id → id (deep, handles $and/$or/$not)
// ---------------------------------------------------------------------------
export function mapFilterOutbound(filter: PlainObject): PlainObject {
  const result: PlainObject = {};
  for (const [key, val] of Object.entries(filter)) {
    if (key === "_id") {
      result["id"] = val;
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

// ---------------------------------------------------------------------------
// Aggregate pipeline translation: MongoDB → native format
// ---------------------------------------------------------------------------

function stripDollar(val: string): string {
  return val.startsWith("$") ? val.slice(1) : val;
}

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

export function translateAggregatePipeline(
  pipeline: PlainObject[]
): PlainObject[] {
  return pipeline.map(translateStage);
}
