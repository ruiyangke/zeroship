import {
  ValidationError,
  OptimisticLockError,
  mapOptimisticConcurrencyError,
} from "../errors";
import { trackCollectionAccess } from "../live";
import { IdLoader } from "../loader";
import {
  requireBoundNativeCapability,
  requireNativeCapability,
  type NativeCollection,
} from "../native";
import { Query } from "../query";
import type { NormalizedSchema } from "../schema";
import {
  mapResultDoc,
  mapDocOutbound,
  mapFilterOutbound,
  mapUpdateOutbound,
  translateAggregatePipeline,
} from "../utils";
import {
  validateDoc,
  checkPartial,
  isArrayElement,
} from "../validate";
import {
  type Actor,
  type Filter,
  type FieldDef,
  type Id,
  type NamedIndexSpec,
  type PlainObject,
  type Result,
  type Row,
  type RowInput,
  type UpsertOptions,
  type UpdateExpression,
  type WithRelations,
  type WithSpec,
  err,
  ok,
} from "../types";
import { validateEncryptedFieldsInFilter } from "./encryption-fence";
import { _maybeWarnUnindexedFilter } from "./index-warnings";

export interface CrudCollectionInternals<
  S = PlainObject,
  N extends string = string,
  AllSchemas extends Record<string, unknown> = Record<string, unknown>,
> {
  _name: string;
  _schema: NormalizedSchema;
  _softDelete: boolean;
  _versioning: boolean;
  _indexes: readonly NamedIndexSpec[];
  _knownFields: Set<string>;
  _idLoader: IdLoader<Row<S>> | null;
  _txDepth: number;
  _run<T>(fn: () => Promise<T>): Promise<Result<T>>;
  _toResultError(e: unknown): Error;
  _loadById(id: string, txDepthAtCall: number): Promise<Row<S> | null>;
  _nativeCollection(): NativeCollection;
  _toColumn(field: string): string;
  _toField(column: string): string;
  _loadRelations(rows: PlainObject[], withSpec: WithSpec): Promise<void>;
}

/**
 * Extracts the plain field map from an update argument for validation.
 * Handles both `{ $set: { field: val } }` and bare `{ field: val }` styles.
 * `$push`, `$addToSet`, `$inc`, `$dec`, `$mul`, and other operators are excluded.
 */
function extractUpdateFields(update: PlainObject): PlainObject {
  const fields: PlainObject = {};
  for (const [key, val] of Object.entries(update)) {
    if (key === "$set" && typeof val === "object" && val !== null) {
      for (const k of Object.keys(val as PlainObject)) {
        if (k === "__proto__" || k === "constructor" || k === "prototype") {
          continue;
        }
        fields[k] = (val as PlainObject)[k];
      }
    } else if (!key.startsWith("$")) {
      if (
        typeof val === "object" &&
        val !== null &&
        !Array.isArray(val) &&
        Object.keys(val as PlainObject).every((k) => k.startsWith("$"))
      ) {
        continue;
      }
      fields[key] = val;
    }
  }
  return fields;
}

/**
 * Validates $push / $addToSet values against the schema's array item type.
 * Throws ValidationError if any pushed value does not match the declared items type.
 * Numeric operators ($inc, $dec, $mul) are skipped — they are inherently numeric.
 *
 * Exported for in-process regression tests (see `r5-array-item-validation.test.ts`).
 * Production callers go through the collection update path.
 */
export function validateArrayPushOps(
  update: PlainObject,
  schema: NormalizedSchema,
): void {
  for (const op of ["$push", "$addToSet"] as const) {
    const opVal = update[op];
    if (opVal === null || typeof opVal !== "object") continue;

    for (const [field, val] of Object.entries(opVal as PlainObject)) {
      const def = schema[field];
      if (!def || def.type !== "array" || !def.items) continue;
      const itemType = def.items;

      if (!isArrayElement(itemType, val)) {
        throw new ValidationError({
          [field]: {
            path: field,
            message: `${op} value for ${field} must be a ${itemType}`,
          },
        });
      }
    }
  }
}

/**
 * D4 — return the caller-supplied `version: N` value from a filter,
 * but only when versioning is enabled on this collection AND the
 * value is a plain number (not a `$gt`/`$in`/etc. operator). Returns
 * `null` otherwise so callers can short-circuit to the non-CAS path.
 */
export function extractCasVersion(
  schema: Record<string, FieldDef>,
  filter: PlainObject,
): number | null {
  const column = Object.keys(schema).find((key) => schema[key].concurrency === true);
  if (column === undefined) return null;
  if (filter === null || typeof filter !== "object") return null;
  const v = filter[column];
  if (typeof v === "number" && Number.isFinite(v)) return v;
  return null;
}

export function insertCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  row: RowInput<S>,
): Promise<Result<Row<S>>> {
  return self._run(async () => {
    const validated = validateDoc(row as PlainObject, self._schema);
    const outbound = mapDocOutbound(validated, self._toColumn);
    const result = await self
      ._nativeCollection()
      .insert(outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>);
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function insertManyCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  rows: RowInput<S>[],
): Promise<Result<Row<S>[]>> {
  if (rows.length === 0) return Promise.resolve(ok([] as Row<S>[]));
  return self._run(async () => {
    const validated = (rows as PlainObject[]).map((r) =>
      validateDoc(r, self._schema),
    );
    const outbound = validated.map((r) => mapDocOutbound(r, self._toColumn));
    const results = await self
      ._nativeCollection()
      .insertMany(
        outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>[],
      );
    return (results ?? []).map((r) =>
      mapResultDoc(r as PlainObject, self._toField),
    ) as Row<S>[];
  });
}

export function getCollection<
  S,
  N extends string,
  AllSchemas extends Record<string, unknown>,
>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  idOrFilter: string | Id<N> | Filter<S>,
  opts: {
    actor?: Actor;
    select?: (string & keyof Row<S>)[];
    orderBy?: Record<string, 1 | -1>;
    unmask?: (string & keyof Row<S>)[];
    unmaskReason?: string;
    with?: WithSpec;
  } = {},
): Promise<Result<Row<S> | null>> {
  trackCollectionAccess(self._name);
  const txDepthAtCall = self._txDepth;
  const isBareId = typeof idOrFilter === "string";
  if (
    isBareId &&
    opts.select === undefined &&
    opts.orderBy === undefined &&
    opts.unmask === undefined &&
    opts.actor === undefined &&
    opts.unmaskReason === undefined &&
    opts.with === undefined &&
    txDepthAtCall === 0
  ) {
    return self._run(() => self._loadById(idOrFilter, txDepthAtCall));
  }
  return self._run(async () => {
    const filter = isBareId ? ({ id: idOrFilter } as unknown as Filter<S>) : idOrFilter;
    if (!isBareId) {
      validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
      _maybeWarnUnindexedFilter(
        self._name,
        self._schema,
        filter as PlainObject,
        self._indexes,
      );
    }
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const nativeOpts: ZeroshipDbFindOpts = { limit: 1 };
    if (opts.select !== undefined) {
      nativeOpts.select = opts.select.map((f) => self._toColumn(f));
    }
    if (opts.orderBy !== undefined) {
      const mappedOrder: Record<string, 1 | -1> = {};
      for (const [k, v] of Object.entries(opts.orderBy)) {
        mappedOrder[self._toColumn(k)] = v as 1 | -1;
      }
      nativeOpts.orderBy = mappedOrder;
    }
    if (opts.unmask !== undefined) {
      nativeOpts.unmask = opts.unmask.map((f) => self._toColumn(f));
    }
    if (opts.actor !== undefined) {
      nativeOpts.actor = opts.actor;
    }
    if (opts.unmaskReason !== undefined) {
      nativeOpts.unmaskReason = opts.unmaskReason;
    }
    const rows = (await self._nativeCollection().find(mapped, nativeOpts)) ?? [];
    if (rows.length === 0) return null;
    const row = mapResultDoc(rows[0] as PlainObject, self._toField);
    if (opts.with !== undefined) {
      await self._loadRelations([row], opts.with);
    }
    return row as Row<S>;
  });
}

export async function loadByIdCollection<
  S,
  N extends string,
  AllSchemas extends Record<string, unknown>,
>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  id: string,
  txDepthAtCall: number,
): Promise<Row<S> | null> {
  if (self._idLoader === null) {
    self._idLoader = new IdLoader<Row<S>>(
      async (ids) => {
        const filter: ZeroshipDbFilter = mapFilterOutbound(
            { id: { $in: ids } } as unknown as ZeroshipDbFilter,
            self._toColumn,
          );
        const rows = (await self._nativeCollection().find(filter, {})) ?? [];
        const map = new Map<string, Row<S>>();
        for (const r of rows) {
          const mapped = mapResultDoc(r as PlainObject, self._toField) as Row<S>;
          map.set(String((mapped as PlainObject)["id"]), mapped);
        }
        return map;
      },
      () => self._txDepth,
    );
  }
  return self._idLoader.load(id, txDepthAtCall);
}

export async function existsCollection<
  S,
  N extends string,
  AllSchemas extends Record<string, unknown>,
>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<boolean>> {
  trackCollectionAccess(self._name);
  try {
    validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  } catch (e) {
    return err(self._toResultError(e));
  }
  try {
    const { data, error } = await findCollection(self, filter).limit(1);
    if (error) return err(error);
    return ok((data?.length ?? 0) > 0);
  } catch (e) {
    return err(self._toResultError(e));
  }
}

export function findCollection<
  S,
  N extends string,
  AllSchemas extends Record<string, unknown>,
>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
  opts?: {
    actor?: Actor;
    unmask?: (string & keyof Row<S>)[];
    unmaskReason?: string;
    with?: WithSpec;
  },
): Query<S, Row<S>, AllSchemas> {
  trackCollectionAccess(self._name);
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  _maybeWarnUnindexedFilter(
    self._name,
    self._schema,
    filter as PlainObject,
    self._indexes,
  );
  const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
  const q = new Query<S, Row<S>, AllSchemas>(
    self._name,
    mapped,
    async (_col, f, fopts) => self._nativeCollection().find(f, fopts),
    self._toField,
    self._toColumn,
    (rows, spec) => self._loadRelations(rows, spec),
    opts?.unmask !== undefined || opts?.actor !== undefined || opts?.unmaskReason !== undefined
      ? {
          unmask: opts.unmask?.map((f) => self._toColumn(f)),
          actor: opts.actor,
          unmaskReason: opts.unmaskReason,
        }
      : undefined,
    self._schema,
  );
  if (opts?.with !== undefined) q.with(opts.with);
  return q;
}

export function upsertCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  row: RowInput<S>,
  options: UpsertOptions<S>,
): Promise<Result<Row<S>>> {
  return self._run(async () => {
    const invalid = (message: string): never => {
      throw new ValidationError({ conflictFields: { path: "conflictFields", message } });
    };
    const fields = options?.conflictFields;
    if (!Array.isArray(fields) || fields.length === 0) {
      invalid("conflictFields must be a non-empty array of application-owned fields");
    }
    const seen = new Set<string>();
    const conflictCols = fields.map((field) => {
      if (typeof field !== "string") invalid("every conflict field must be a string");
      const column = self._toColumn(field);
      if (self._schema[field]?.assign !== undefined) {
        invalid(`upsert conflict field '${field}' is platform-assigned; use an application-owned unique key`);
      }
      if (!Object.hasOwn(self._schema, field)) invalid(`unknown upsert conflict field '${field}'`);
      if (seen.has(column)) invalid(`duplicate upsert conflict field '${field}'`);
      seen.add(column);
      return column;
    });
    const validated = validateDoc(row as PlainObject, self._schema);
    const outbound = mapDocOutbound(validated, self._toColumn);
    for (const column of conflictCols) {
      if (!Object.hasOwn(outbound, column)) invalid(`upsert conflict field '${column}' must be supplied in the document`);
    }
    const result = await self._nativeCollection().upsert(
      outbound as Record<string, ZeroshipScalar | ZeroshipScalar[]>,
      { conflictFields: conflictCols },
    );
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function updateCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  idOrFilter: string | Filter<S>,
  patch: UpdateExpression<S>,
): Promise<Result<Row<S> | null>> {
  return self._run(async () => {
    const isBareId = typeof idOrFilter === "string";
    const filter = isBareId ? ({ id: idOrFilter } as unknown as Filter<S>) : idOrFilter;
    if (!isBareId) {
      validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
    }
    const updateObj = patch as PlainObject;
    const fields = extractUpdateFields(updateObj);
    checkPartial(fields, self._schema);
    validateArrayPushOps(updateObj, self._schema);
    const casVersion = extractCasVersion(self._schema, filter as PlainObject);
    const mappedFilter = mapFilterOutbound(
      filter as ZeroshipDbFilter,
      self._toColumn,
    );
    const mappedUpdate = mapUpdateOutbound(updateObj, self._toColumn);
    let result;
    try {
      result = await self._nativeCollection().update(mappedFilter, mappedUpdate);
    } catch (e) {
      if (casVersion !== null) {
        throw mapOptimisticConcurrencyError(e, self._name, casVersion);
      }
      throw e;
    }
    if (result === null) {
      if (casVersion !== null) {
        throw new OptimisticLockError(casVersion, self._name);
      }
      return null;
    }
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function updateManyCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
  update: UpdateExpression<S>,
): Promise<Result<{ count: number }>> {
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  return self._run(async () => {
    const updateObj = update as PlainObject;
    const fields = extractUpdateFields(updateObj);
    checkPartial(fields, self._schema);
    validateArrayPushOps(updateObj, self._schema);
    const casVersion = extractCasVersion(self._schema, filter as PlainObject);
    const mappedFilter = mapFilterOutbound(
      filter as ZeroshipDbFilter,
      self._toColumn,
    );
    const mappedUpdate = mapUpdateOutbound(updateObj, self._toColumn);
    let n: number;
    try {
      n = await self._nativeCollection().updateMany(mappedFilter, mappedUpdate);
    } catch (e) {
      if (casVersion !== null) {
        throw mapOptimisticConcurrencyError(e, self._name, casVersion);
      }
      throw e;
    }
    if (n === 0 && casVersion !== null) {
      throw new OptimisticLockError(casVersion, self._name);
    }
    return { count: n };
  });
}

export function deleteCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  idOrFilter: string | Filter<S>,
): Promise<Result<Row<S> | null>> {
  return self._run(async () => {
    const isBareId = typeof idOrFilter === "string";
    const filter = isBareId ? ({ id: idOrFilter } as unknown as Filter<S>) : idOrFilter;
    if (!isBareId) {
      validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
    }
    const casVersion = extractCasVersion(self._schema, filter as PlainObject);

    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const result = await self._nativeCollection().delete(mapped);
    if (result === null) {
      if (casVersion !== null) throw new OptimisticLockError(casVersion, self._name);
      return null;
    }
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function deleteManyCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<{ deletedCount: number }>> {
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  _maybeWarnUnindexedFilter(
    self._name,
    self._schema,
    filter as PlainObject,
    self._indexes,
  );
  return self._run(async () => {
    const casVersion = extractCasVersion(self._schema, filter as PlainObject);

    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const n = await self._nativeCollection().deleteMany(mapped);
    if (n === 0 && casVersion !== null) {
      throw new OptimisticLockError(casVersion, self._name);
    }
    return { deletedCount: n };
  });
}

export function purgeCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  idOrFilter: string | Filter<S>,
): Promise<Result<Row<S> | null>> {
  return self._run(async () => {
    const isBareId = typeof idOrFilter === "string";
    const filter = isBareId ? ({ id: idOrFilter } as unknown as Filter<S>) : idOrFilter;
    if (!isBareId) {
      validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
    }
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const purge = requireBoundNativeCapability(self._nativeCollection(), "purge", {
      code: "PURGE_NOT_AVAILABLE",
      message:
        "@zeroship/db: env.db.<collection>.purge not available — " +
        "runtime is missing the P7 PR 5 purge surface.",
    });
    const result = await purge(mapped);
    if (result === null) return null;
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function purgeManyCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<{ purgedCount: number }>> {
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  _maybeWarnUnindexedFilter(
    self._name,
    self._schema,
    filter as PlainObject,
    self._indexes,
  );
  return self._run(async () => {
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const purgeMany = requireBoundNativeCapability(
      self._nativeCollection(),
      "purgeMany",
      {
        code: "PURGE_NOT_AVAILABLE",
        message:
          "@zeroship/db: env.db.<collection>.purgeMany not available — " +
          "runtime is missing the P7 PR 5 purge surface.",
      },
    );
    const n = await purgeMany(mapped);
    return { purgedCount: n };
  });
}

export function restoreCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  idOrFilter: string | Filter<S>,
): Promise<Result<Row<S> | null>> {
  return self._run(async () => {
    const isBareId = typeof idOrFilter === "string";
    const filter = isBareId ? ({ id: idOrFilter } as unknown as Filter<S>) : idOrFilter;
    if (!isBareId) {
      validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
    }
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const restore = requireBoundNativeCapability(self._nativeCollection(), "restore", {
      code: "RESTORE_NOT_AVAILABLE",
      message:
        "@zeroship/db: env.db.<collection>.restore not available — " +
        "runtime is missing the P7 PR 5 restore surface.",
    });
    const result = await restore(mapped);
    if (result === null) return null;
    return mapResultDoc(result as PlainObject, self._toField) as Row<S>;
  });
}

export function restoreManyCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<{ restoredCount: number }>> {
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  _maybeWarnUnindexedFilter(
    self._name,
    self._schema,
    filter as PlainObject,
    self._indexes,
  );
  return self._run(async () => {
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const restoreMany = requireBoundNativeCapability(
      self._nativeCollection(),
      "restoreMany",
      {
        code: "RESTORE_NOT_AVAILABLE",
        message:
          "@zeroship/db: env.db.<collection>.restoreMany not available — " +
          "runtime is missing the P7 PR 5 restore surface.",
      },
    );
    const n = await restoreMany(mapped);
    return { restoredCount: n };
  });
}

export function countCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<number>> {
  trackCollectionAccess(self._name);
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  return self._run(async () => {
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const n = await self._nativeCollection().count(mapped);
    return typeof n === "number" ? n : 0;
  });
}

export function distinctCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  field: string & keyof Row<S>,
  filter: Filter<S> = {} as Filter<S>,
): Promise<Result<(string | number | boolean | null)[]>> {
  trackCollectionAccess(self._name);
  validateEncryptedFieldsInFilter(filter as PlainObject, self._schema);
  {
    const def = self._schema[field as string];
    if (def && def.encrypted) {
      throw Object.assign(
        new Error(
          `distinct("${field}"): encrypted columns are not distinct-able (would leak ciphertext frequencies).`,
        ),
        { code: "DISTINCT_ON_ENCRYPTED_FIELD_UNSUPPORTED" as const },
      );
    }
  }
  return self._run(async () => {
    if (!self._knownFields.has(field)) {
      throw new ValidationError({
        [field]: { path: field, message: `unknown field: ${field}` },
      });
    }
    const mapped = mapFilterOutbound(filter as ZeroshipDbFilter, self._toColumn);
    const column = self._toColumn(field);
    const result = await self._nativeCollection().distinct(mapped, { field: column });
    return result ?? [];
  });
}

export function aggregateCollection<S, N extends string, AllSchemas extends Record<string, unknown>>(
  self: CrudCollectionInternals<S, N, AllSchemas>,
  pipeline: ZeroshipDbAggregateStage[],
): Promise<Result<PlainObject[]>> {
  trackCollectionAccess(self._name);
  return self._run(async () => {
    const translated = translateAggregatePipeline(
      pipeline as unknown as PlainObject[],
      self._toColumn,
    ) as ZeroshipDbAggregateStage[];
    const results = await self._nativeCollection().aggregate(translated);
    return (results ?? []).map((d) =>
      mapResultDoc(d as PlainObject, self._toField),
    );
  });
}
