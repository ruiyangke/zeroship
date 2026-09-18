/**
 * Test-only adapter that stands in for the descriptor build step.
 *
 * `installSchema` is supplied by the crate-owned test adapter and takes its
 * collections exclusively from the runtime schema descriptor.
 * In production the toolchain folds committed migrations into that
 * descriptor before boot.
 *
 * Tests here have no migrations to fold — they declare a schema inline and
 * assert on Collection/Query behaviour. So this adapter does what the
 * toolchain does: it derives a descriptor from the declared schema and hands
 * that to the installer. Without it the installer is correct to install
 * nothing, and every downstream assertion fails as `db.<collection>` being
 * undefined, a symptom nowhere near its cause.
 */
import {
  installSchema,
  type InstallSchemaOptions,
  type ProjectedCollection,
  type SchemaProjection,
} from "../../../crates/zeroship-data-v8/js/testing.js";
// Use the source entry throughout SDK unit tests so branded builders and the
// crate-owned adapter resolve one implementation instance. The separate
// package-surface test exercises the compiled public artifact.
import { SchemaBuilder, t } from "../src/index.js";
import type { Db, FieldDef, SchemaInput } from "../src/index.js";
import type { NativeDb } from "../src/native.js";

export type FieldDeclaration = FieldDef | { toFieldDef(): FieldDef };

/**
 * Map a declared field record to the decoded `FieldDef` map a projection
 * carries: `t.*` builders unwrap via `.toFieldDef()`, already-decoded
 * `FieldDef`s pass through. Test-only stand-in for the Rust descriptor
 * fold's field step.
 */
export function fieldsOf(
  schema: Record<string, FieldDeclaration>,
): Record<string, FieldDef> {
  const out: Record<string, FieldDef> = {};
  for (const [key, value] of Object.entries(schema)) {
    out[key] = "toFieldDef" in value ? { ...value.toFieldDef() } : { ...value };
  }
  return out;
}

export const generatedSchema = {
  id: t.string().required().primaryKey().assigned({ by: "typedId", on: "insert" }),
  created_at: t.timestamp().required().assigned({ by: "now", on: "insert" }),
  updated_at: t.timestamp().required().assigned({ by: "now", on: "write" }),
  created_by: t.string().nullable().required().assigned({ by: "actor", on: "insert" }),
  updated_by: t.string().nullable().required().assigned({ by: "actor", on: "write" }),
  version: t.number().required().assigned({ by: "increment(1)", on: "write" }),
  deleted_at: t.timestamp().nullable().required().assigned({ by: "now", on: "delete" }),
};

type FixtureSchemas<T> = {
  [K in keyof T]: T[K] extends SchemaBuilder<infer F> ? SchemaBuilder<F & typeof generatedSchema> : T[K] & typeof generatedSchema
};

/**
 * Build the projection the host would emit for `schemas`: the decoded
 * `FieldDef` map plus the declared named indexes, with no per-collection
 * options (the installer no longer reads them).
 *
 * A declaration is either a `SchemaBuilder` — which carries named indexes
 * alongside its fields — or a bare field record, which carries only fields.
 *
 * Exported for tests that call `installSchema` directly to exercise install
 * mechanics (reserved names and re-entrancy) rather
 * than Collection behaviour. Those still need a projection, or the installer
 * has no collections to apply the mechanics to and the assertions pass
 * vacuously.
 */
export function descriptorFor(schemas: Record<string, unknown>): SchemaProjection {
  const collections = Object.create(null) as Record<string, ProjectedCollection>;

  for (const [name, declared] of Object.entries(schemas)) {
    const builder = declared instanceof SchemaBuilder ? declared : null;
    const fields = builder ? builder.fields : declared;
    const options = builder?.options;

    const normalized = fieldsOf(fields as Record<string, FieldDeclaration>);
    const generated = fieldsOf(generatedSchema);
    if (options?.softDelete) generated.deleted_at.softDelete = true;
    if (options?.versioning) generated.version.concurrency = true;
    collections[name] = {
      // The projection carries decoded wire FieldDefs, not `t.*` builders.
      fields: { ...generated, ...normalized },
      indexes: (builder?.indexes ?? []).map((idx) => ({
        name: idx.name,
        fields: [...idx.fields],
        ...(idx.unique ? { unique: true } : {}),
      })),
    };
  }

  return { collections };
}

export function installSchemaForTest<
  const T extends Record<string, SchemaInput>,
>(
  schemas: T,
  opts: { native: NativeDb; naming?: InstallSchemaOptions["naming"] },
): Db<FixtureSchemas<T>> {
  installSchema(opts.native, descriptorFor(schemas), { naming: opts.naming });
  // The installer plants the per-collection wrappers and the `transaction` /
  // `live` extensions on the native handle as own properties, so the handle
  // itself is the `Db<T>` the call sites (`db.users.find(...)`) expect.
  return opts.native as unknown as Db<FixtureSchemas<T>>;
}
