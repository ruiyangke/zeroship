import { trackCollectionAccess } from "./live";
import type { NormalizedSchema } from "../../../../packages/db/src/schema";
import type { NamingStrategy, PlainObject } from "../../../../packages/db/src/types";
import { mapResultDoc } from "./utils";

type RelationSelection = Readonly<Record<string, true | undefined>>;
export type ReadResultMapper = (row: PlainObject, withSpec?: RelationSelection) => PlainObject;

/** Map loaded rows using descriptors, leaving JSON and protected values opaque. */
export function createReadResultMapper(
  schema: NormalizedSchema,
  schemas: Readonly<Record<string, NormalizedSchema>>,
  naming: NamingStrategy,
  toField: (column: string) => string,
): ReadResultMapper {
  const targetMappers = new Map<string, (column: string) => string>();
  const targets = new Map(Object.values(schema).filter(field => field.relation).map(field => [field.relation, field.refTarget]));
  return (row, withSpec) => {
    const mapped = mapResultDoc(row, toField);
    for (const alias of Object.keys(withSpec ?? {})) {
      const target = targets.get(alias);
      const value = row[alias];
      if (!target || !schemas[target] || value === null || typeof value !== "object" || Array.isArray(value)) continue;
      let targetMapper = targetMappers.get(target);
      if (!targetMapper) {
        const fields = new Map(Object.keys(schemas[target]).map(key => [naming.toColumn(key), key]));
        targetMapper = column => fields.get(column) ?? column;
        targetMappers.set(target, targetMapper);
      }
      mapped[alias] = mapResultDoc(value as PlainObject, targetMapper);
    }
    return mapped;
  };
}

export function trackRelations(schema: NormalizedSchema, spec: RelationSelection): void {
  for (const field of Object.values(schema)) {
    if (field.relation && field.refTarget && Object.hasOwn(spec, field.relation)) {
      trackCollectionAccess(field.refTarget);
    }
  }
}
