import type { NormalizedSchema } from "../schema";
import type { Filter, PlainObject, Result, WithSpec } from "../types";
import { readTransactionDepth } from "../tx-state.js";
import { MAX_ID_BATCH } from "../membership-cap.js";

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
 *   1. Walk the schema; the key must carry a `refTarget` — set either by
 *      `t.ref(...)` or by a migration's `t.text().references(...)`.
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
  const load = async ([field, spec]: [string, unknown]): Promise<void> => {
    if (spec !== true) {
      throw Object.assign(
        new Error(
          `find/get: with: { ${field}: ${JSON.stringify(spec)} } — only \`true\` is supported in v1`,
        ),
        { code: "WITH_UNSUPPORTED_VALUE" as const },
      );
    }
    // A relation is identified by its relation METADATA (`refTarget`), not
    // by the `type` token, which describes storage.
    //
    // This used to require `fieldDef.type === "ref"`, and that made `with`
    // unusable on the migration-first pipeline — the platform's only schema
    // path. A committed migration declares a foreign key as
    // `t.text().references("users", "id")` (the vendored engine DSL has no
    // `t.ref()`), and the engine's descriptor reports it honestly as
    // `{ type: "string", refTarget: "users", refColumn: "id" }`. The FK is
    // created in the database and enforced — measured on
    // examples/db-todos: an orphan insert fails with `FOREIGN KEY constraint
    // failed` / `FOREIGN_KEY_VIOLATION` — but `with: { userId: true }` threw
    //
    //     "userId" is not a t.ref field on "todos"
    //
    // so every eager-load against a migration-declared FK was dead. Only a
    // hand-written `defineSchema` using `t.ref()` could ever satisfy the old
    // gate, and that is not how creator schemas are authored.
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
    // Chunked, because the native builder REJECTS a membership list longer
    // than MAX_MEMBERSHIP_LIST_LEN (`zeroship-data-sql/src/compile.rs`). Sending
    // the whole deduplicated set failed outright for any page carrying more
    // than that many DISTINCT foreign keys - which an unpaginated find()
    // reaches easily, so a documented feature broke on ordinary data.
    //
    // Deliberately sequential rather than Promise.all: the relations
    // can run concurrently outside a transaction, and fanning out here
    // too would multiply in-flight queries by the chunk count for a single
    // creator call.
    const byId = new Map<string, PlainObject>();
    for (let i = 0; i < ids.length; i += MAX_ID_BATCH) {
      const chunk = ids.slice(i, i + MAX_ID_BATCH);
      const { data: targetRows, error } = await targetCol.find({
        id: { $in: chunk },
      } as Filter<unknown>);
      if (error) throw error;
      for (const tr of (targetRows ?? []) as PlainObject[]) {
        const tid = tr.id;
        if (typeof tid === "string") {
          byId.set(tid, tr);
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
        typeof v === "string" && v.length > 0 ? (byId.get(v) ?? null) : null;
    }
  };
  const entries = Object.entries(withSpec);
  if (readTransactionDepth(self) > 0) {
    for (const entry of entries) await load(entry);
  } else {
    await Promise.all(entries.map(load));
  }
}
