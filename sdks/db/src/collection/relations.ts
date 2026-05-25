import type { NormalizedSchema } from "../schema.js";
import type { Filter, PlainObject, Result, WithSpec } from "../types.js";

interface RelationTargetCollection {
  find(filter: Filter<unknown>): PromiseLike<Result<unknown[]>>;
}

export interface RelationsCollectionInternals {
  _schema: NormalizedSchema;
  _name: string;
  _resolveCollection:
    | ((name: string) => RelationTargetCollection | undefined)
    | null;
}

/**
 * @internal — eager-load referenced rows for each `with` key onto every
 * parent row. Mutates the rows in place. Used by both the `get` and
 * `find` paths so the relation-loading logic lives in one place.
 *
 * Per `with` key:
 *   1. Walk the schema; the key must be a `t.ref(...)` field.
 *   2. Resolve the target Collection via the planted `_resolveCollection`.
 *   3. Dedupe foreign ids across the parent rows.
 *   4. Fire ONE `find({id: {$in: [...]}})` against the target.
 *   5. Build an id→row map; the joined row replaces the FK number at
 *      the same key (null for null FK or missing target row).
 *
 * v1 limitation: the joined row overwrites the FK number at the same
 * key. To keep both, declare the FK on a separate field — e.g.
 * `user: t.ref("users")` instead of `userId: t.ref("users")` — and the
 * number lives on the joined row as `user.id`.
 */
export async function loadRelations(
  self: RelationsCollectionInternals,
  rows: PlainObject[],
  withSpec: WithSpec,
): Promise<void> {
  if (rows.length === 0) return;
  await Promise.all(
    Object.entries(withSpec).map(async ([field, spec]) => {
      if (spec !== true) {
        throw Object.assign(
          new Error(
            `find/get: with: { ${field}: ${JSON.stringify(spec)} } — only \`true\` is supported in v1`,
          ),
          { code: "WITH_UNSUPPORTED_VALUE" as const },
        );
      }
      const fieldDef = self._schema[field];
      if (!fieldDef || fieldDef.type !== "ref") {
        throw Object.assign(
          new Error(
            `find/get: with: { ${field}: true } — "${field}" is not a t.ref field on "${self._name}"`,
          ),
          { code: "WITH_NOT_A_REF_FIELD" as const },
        );
      }
      const targetName = fieldDef.refTarget;
      if (typeof targetName !== "string" || targetName.length === 0) {
        throw Object.assign(
          new Error(
            `find/get: with: { ${field}: true } — "${field}" has no refTarget`,
          ),
          { code: "WITH_MISSING_REF_TARGET" as const },
        );
      }
      const resolve = self._resolveCollection;
      if (resolve === null) {
        throw Object.assign(
          new Error(
            `find/get: with: { ${field}: true } — this Collection was created via model() without a parent db, ` +
              `so sibling collections cannot be resolved. Declare the schema via "export default { schema }" to enable relation loading.`,
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
      const ids: string[] = [];
      const seen = new Set<string>();
      for (const r of rows) {
        const v = r[field];
        if (v === null || v === undefined) continue;
        if (typeof v !== "string") {
          throw Object.assign(
            new TypeError(
              `_loadRelations: FK value for field '${field}' must be a string id (got ${typeof v})`,
            ),
            { code: "WITH_FK_NOT_ID_SHAPED" as const },
          );
        }
        if (v.length === 0) continue;
        if (!seen.has(v)) {
          seen.add(v);
          ids.push(v);
        }
      }
      if (ids.length === 0) {
        for (const r of rows) r[field] = null;
        return;
      }
      const { data: targetRows, error } = await targetCol.find({
        id: { $in: ids },
      } as Filter<unknown>);
      if (error) throw error;
      const byId = new Map<string, PlainObject>();
      for (const tr of (targetRows ?? []) as PlainObject[]) {
        const tid = tr.id;
        if (typeof tid === "string") {
          byId.set(tid, tr);
        }
      }
      for (const r of rows) {
        const v = r[field];
        if (v === null || v === undefined) {
          r[field] = null;
          continue;
        }
        r[field] =
          typeof v === "string" && v.length > 0 ? (byId.get(v) ?? null) : null;
      }
    }),
  );
}
