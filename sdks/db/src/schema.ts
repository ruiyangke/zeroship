/**
 * Schema normalization: converts an `export default { schema: { … } }`
 * field map of `TypeBuilder` instances (or a top-level `t.union(...)`)
 * into the canonical `NormalizedSchema` shape consumed by the rest of
 * the SDK.
 */
import { TypeBuilder, SchemaBuilder, FieldDef, PlainObject } from "./types.js";

/** A normalized schema mapping field names to their FieldDef. */
export type NormalizedSchema = Record<string, FieldDef>;

/** Input form: a record of TypeBuilder instances. Field values must be
 *  produced by the `t.*` API (`t.string()`, `t.number()`, etc.). */
type SchemaInput = Record<string, TypeBuilder<any, any>>;

/**
 * C2 — collection-level discriminated union. When the value of a key
 * in the schema map is `t.union(...)` directly, the collection is a
 * discriminated union: every row is one of the declared variants. The
 * SDK expands this into flat columns per the proposal (§C2) — see
 * `normalizeSchema` / `expandUnionToFlatColumns`.
 */
type UnionSchemaInput = TypeBuilder<any, any>;

/** Accepts either a record of fields OR a top-level union TypeBuilder. */
type SchemaInputOrUnion = SchemaInput | UnionSchemaInput;

/**
 * @internal
 * Converts a SchemaInput into a NormalizedSchema. Every field value
 * must be a `TypeBuilder` produced by the `t.*` API (or the whole
 * input may be a single top-level `t.union(...)` — proposal §C2).
 * Any other shape throws.
 */
export function normalizeSchema(input: SchemaInputOrUnion): NormalizedSchema {
  // C2 — top-level discriminated union. The whole collection's row
  // shape is a `t.union(...)`, expanded into flat columns + discriminator.
  if (input instanceof TypeBuilder) {
    const def = input.toFieldDef();
    if (def.type === "union") {
      return expandUnionToFlatColumns(def);
    }
    throw new Error(
      `normalizeSchema: top-level TypeBuilder must be a t.union(...) (got type "${def.type}")`,
    );
  }
  const result: NormalizedSchema = {};

  for (const [key, rawVal] of Object.entries(input)) {
    if (!(rawVal instanceof TypeBuilder)) {
      throw new Error(
        `unrecognized schema field "${key}": every field must be a t.* builder ` +
          `(e.g. t.string(), t.number(), t.ref("users")). Bare constructors and ` +
          `Mongoose-style { type: Constructor } objects are no longer supported.`,
      );
    }
    // TypeBuilder instances are structurally complete (including the
    // B2 ref* fields: refTarget, onDelete, onUpdate, deferrable).
    // Shallow-clone is correct: every member is a plain value or a
    // function (`default`), never a mutable container.
    result[key] = { ...rawVal.toFieldDef() };
  }

  return result;
}

/**
 * C2 — expand a top-level `t.union(...)` into a flat `NormalizedSchema`.
 *
 * - Each non-discriminator field that appears in any variant becomes a
 *   top-level nullable column.
 * - Fields that appear in multiple variants must agree on their `type`;
 *   nullability is the **loosest** (a field required in one variant
 *   and optional in another collapses to optional at the column level,
 *   since at least one variant can produce a row without it). Per-
 *   variant required-ness is preserved in the variant shape so the
 *   DDL emitter can add CHECK constraints (e.g.
 *   `kind <> 'login' OR (userId IS NOT NULL AND ip IS NOT NULL)`).
 * - The discriminator field becomes a NOT NULL column with an `enum`
 *   constraint listing every variant's literal value. The field
 *   also carries `variants` (the raw per-variant shape map) and
 *   `discriminator: "__discriminator__"` so the DDL emitter / diff
 *   engine know it's a flat-expanded union.
 */
export function expandUnionToFlatColumns(def: FieldDef): NormalizedSchema {
  if (def.type !== "union" || def.variants === undefined || def.discriminator === undefined) {
    throw new Error("expandUnionToFlatColumns: not a union FieldDef");
  }
  const discriminator = def.discriminator;
  const variants = def.variants;
  const result: NormalizedSchema = {};

  // 1. Discriminator column. The literal value of `discriminator` in
  //    each variant becomes a member of the enum. The discriminator's
  //    primitive type is inferred from the first variant's literal —
  //    every variant must agree (validated below).
  const discValues: (string | number | boolean)[] = [];
  let discPrimType: "string" | "number" | "boolean" | null = null;
  for (let i = 0; i < variants.length; i++) {
    const fd = variants[i][discriminator];
    if (fd === undefined || fd.type !== "literal" || fd.literalValue === undefined) {
      throw new Error(
        `expandUnionToFlatColumns: variant #${i} missing discriminator field "${discriminator}"`,
      );
    }
    const lit = fd.literalValue;
    const primTy = typeof lit;
    if (primTy !== "string" && primTy !== "number" && primTy !== "boolean") {
      throw new Error(
        `expandUnionToFlatColumns: discriminator literal of variant #${i} has unsupported type "${primTy}"`,
      );
    }
    if (discPrimType === null) {
      discPrimType = primTy as "string" | "number" | "boolean";
    } else if (discPrimType !== primTy) {
      throw new Error(
        `expandUnionToFlatColumns: discriminator literals across variants must share a primitive type (got "${discPrimType}" and "${primTy}")`,
      );
    }
    discValues.push(lit);
  }
  // Defensive: detectDiscriminator already enforces distinctness, but
  // the wire-format variant store is JSON-cloned in places; re-check.
  const seen = new Set<string>();
  for (const v of discValues) {
    const tag = typeof v + ":" + String(v);
    if (seen.has(tag)) {
      throw new Error(
        `expandUnionToFlatColumns: duplicate discriminator value ${JSON.stringify(v)}`,
      );
    }
    seen.add(tag);
  }

  result[discriminator] = {
    type: discPrimType ?? "string",
    required: true,
    enum: discValues as (string | number)[],
    discriminator: "__discriminator__",
    variants: variants.map((v) => {
      const cloned: Record<string, FieldDef> = {};
      for (const [k, fd] of Object.entries(v)) cloned[k] = { ...fd };
      return cloned;
    }),
  };

  // 2. All non-discriminator fields. Each variant's fields become
  //    nullable top-level columns; fields shared across variants must
  //    agree on `type`. We never propagate `required` from a variant
  //    to the column level since not every row will populate it.
  for (const variant of variants) {
    for (const [field, fd] of Object.entries(variant)) {
      if (field === discriminator) continue;
      const existing = result[field];
      if (existing === undefined) {
        // Strip variant-only required marker — at the column level the
        // field is nullable. Per-variant required-ness lives in the
        // discriminator's `variants` and surfaces as CHECK constraints.
        const expanded: FieldDef = { ...fd, required: false };
        result[field] = expanded;
      } else {
        if (existing.type !== fd.type) {
          throw new Error(
            `expandUnionToFlatColumns: field "${field}" has incompatible types across variants ("${existing.type}" vs "${fd.type}")`,
          );
        }
        // Existing already nullable — nothing to do.
      }
    }
  }

  return result;
}

/**
 * B2 — verify that every `t.ref("table")` in `schemas` points at a
 * collection that is itself declared in `schemas`. Throws an `Error`
 * whose `message` is a JSON-encoded envelope with code
 * `ref_target_not_found` on the first violation.
 *
 * The check is intentionally string-based (not type-based) so it acts
 * as a safety net for `t.ref("x" as any)` escapes that bypass the
 * compile-time `Tables<S>` constraint. Cross-app refs are also blocked:
 * the TS literal type for `table` always refers to a key inside the
 * same schema-map literal, so a cross-app reference would have to be
 * smuggled in via `as any`, which this check catches.
 */
export function validateRefTargets(
  schemas: Record<string, unknown>,
): void {
  const declaredCollections = new Set(Object.keys(schemas));
  const reportMissing = (
    collection: string,
    field: string,
    target: string,
  ): never => {
    const message =
      `t.ref("${target}") on ${collection}.${field} — ` +
      `target collection "${target}" is not declared in the schema map. ` +
      `Add "${target}" to the schema map, or fix the typo.`;
    throw Object.assign(new Error(message), {
      code: "ref_target_not_found",
      collection,
      field,
      target,
    });
  };

  for (const [collectionName, rawSchema] of Object.entries(schemas)) {
    // C2 — a top-level `t.union(...)` walks the variants for refs.
    if (rawSchema instanceof TypeBuilder) {
      const fd = rawSchema.toFieldDef();
      if (fd.type === "union" && fd.variants !== undefined) {
        for (const variant of fd.variants) {
          for (const [field, vDef] of Object.entries(variant)) {
            if (vDef.type === "ref" && vDef.refTarget !== undefined && !declaredCollections.has(vDef.refTarget)) {
              reportMissing(collectionName, field, vDef.refTarget);
            }
          }
        }
      }
      continue;
    }
    const fields =
      rawSchema instanceof SchemaBuilder
        ? (rawSchema as SchemaBuilder<Record<string, unknown>>).fields
        : rawSchema;
    if (fields === null || typeof fields !== "object") continue;
    for (const [field, def] of Object.entries(fields as PlainObject)) {
      let refTarget: string | undefined;
      if (def instanceof TypeBuilder) {
        const fd = def.toFieldDef();
        if (fd.type === "ref") refTarget = fd.refTarget;
      } else if (
        def !== null &&
        typeof def === "object" &&
        "type" in (def as PlainObject) &&
        (def as PlainObject).type === "ref"
      ) {
        // Raw FieldDef literal (rare path — `as any` escape).
        refTarget = (def as { refTarget?: string }).refTarget;
      }
      if (refTarget !== undefined && !declaredCollections.has(refTarget)) {
        reportMissing(collectionName, field, refTarget);
      }
    }
  }
}
