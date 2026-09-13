import type { NormalizedSchema } from "../schema";
import type { Filter, IdValue, PlainObject, Result, WithSpec } from "../types";
import { identityKey, isIdentityForField } from "../identity.js";
import { MAX_ID_BATCH } from "../membership-cap.js";

interface RelationTargetCollection {
  _schema: NormalizedSchema;
  find(filter: Filter<unknown>): PromiseLike<Result<unknown[]>>;
}

export interface RelationsCollectionInternals {
  _schema: NormalizedSchema;
  _name: string;
  _resolveCollection:
    | ((name: string) => RelationTargetCollection | undefined)
    | null;
}

/** Eager-load declared references in bounded batches, preserving transaction routing. */
export async function loadRelations(
  self: RelationsCollectionInternals,
  rows: PlainObject[],
  withSpec: WithSpec,
  transactionScoped = false,
): Promise<void> {
  if (rows.length === 0) return;
  const load = async ([field, spec]: [string, unknown]): Promise<void> => {
    if (spec !== true) {
      throw Object.assign(
        new Error(
          `find/get: with: { ${field}: ${JSON.stringify(spec)} } — only \`true\` is supported in v1`,
        ),
        { code: "WITH_UNSUPPORTED_VALUE" as const },
      );
    }
    const fieldDef = self._schema[field];
    if (!fieldDef || typeof fieldDef.refTarget !== "string" || fieldDef.refTarget.length === 0) {
      throw Object.assign(
        new Error(
          `find/get: with: { ${field}: true } — "${field}" is not a reference field on "${self._name}" (no refTarget)`,
        ),
        { code: "WITH_NOT_A_REF_FIELD" as const },
      );
    }
    // The guard above already established a non-empty string, so the former
    // separate WITH_MISSING_REF_TARGET arm here could no longer fire and was
    // removed rather than left as an unreachable branch.
    const targetName = fieldDef.refTarget;
    const resolve = self._resolveCollection;
    if (resolve === null) {
      throw Object.assign(
        new Error(
          `find/get: with: { ${field}: true } — this Collection was created via model() without a parent db, ` +
            `so sibling collections cannot be resolved. Use the generated env.db surface to enable relation loading.`,
        ),
        { code: "WITH_NO_PARENT_DB" as const },
      );
    }
    const targetCol = resolve(targetName);
    if (!targetCol) {
      throw Object.assign(
        new Error(
          `find/get: with: { ${field}: true } — target collection "${targetName}" is not declared on this db`,
        ),
        { code: "WITH_TARGET_NOT_FOUND" as const },
      );
    }
    const ids: IdValue[] = [];
    const seen = new Set<string>();
    for (const r of rows) {
      const v = r[field];
      if (v === null || v === undefined) continue;
      if (!isIdentityForField(v, fieldDef)) {
        throw Object.assign(
          new TypeError(
            `_loadRelations: FK value for field '${field}' must match the declared identity type (got ${typeof v})`,
          ),
          { code: "WITH_FK_NOT_ID_SHAPED" as const },
        );
      }
      if (v === "") continue;
      if (!seen.has(identityKey(v))) {
        seen.add(identityKey(v));
        ids.push(v);
      }
    }
    if (ids.length === 0) {
      for (const r of rows) r[field] = null;
      return;
    }
    // Stay within the ORM membership budget. Run chunks sequentially so one
    // relation load cannot multiply its own in-flight native queries.
    const targetKey = fieldDef.refColumn ?? "id";
    const byId = new Map<string, PlainObject>();
    for (let i = 0; i < ids.length; i += MAX_ID_BATCH) {
      const chunk = ids.slice(i, i + MAX_ID_BATCH);
      const { data: targetRows, error } = await targetCol.find({
        [targetKey]: { $in: chunk },
      } as Filter<unknown>);
      if (error) throw error;
      for (const tr of (targetRows ?? []) as PlainObject[]) {
        const tid = tr[targetKey];
        if (isIdentityForField(tid, targetCol._schema[targetKey])) {
          byId.set(identityKey(tid), tr);
        }
      }
    }
    for (const r of rows) {
      const v = r[field];
      if (v === null || v === undefined) {
        r[field] = null;
        continue;
      }
      r[field] =
        isIdentityForField(v, fieldDef) && v !== "" ? (byId.get(identityKey(v)) ?? null) : null;
    }
  };
  const entries = Object.entries(withSpec);
  if (transactionScoped) {
    for (const entry of entries) await load(entry);
  } else {
    await Promise.all(entries.map(load));
  }
}
