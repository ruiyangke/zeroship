/**
 * Test-only adapter that stands in for the descriptor build step.
 *
 * `installSchema` lives in `@zeroship/bootstrap` (the framework-internal
 * coordination package) and takes its collections exclusively from the
 * runtime schema descriptor. The declared schema argument is not consulted
 * for fields, options, or indexes; an absent descriptor installs nothing.
 * In production the toolchain folds committed migrations into that
 * descriptor before boot.
 *
 * Tests here have no migrations to fold — they declare a schema inline and
 * assert on Collection/Query behaviour. So this adapter does what the
 * toolchain does: it derives a descriptor from the declared schema and hands
 * that to the installer. Without it the installer is correct to install
 * nothing, and every downstream assertion fails as `db.<collection>` being
 * undefined, a symptom nowhere near its cause.
 *
 * The reverse dependency direction (db -> bootstrap) is dev-only; the
 * production graph (bootstrap -> db) stays acyclic.
 */
import {
  installSchema,
  normalizeSchema,
  type InstallSchemaOptions,
  type RuntimeSchemaDescriptor,
} from "@zeroship/bootstrap/install-schema";
// `SchemaBuilder`/`Db`/`SchemaInput` come from the PUBLISHED `@zeroship/db`
// specifier, not raw `../src/...`, because callers build their schemas via
// `schema(...)` imported the same way (`import { schema, t } from
// "@zeroship/db"`). `SchemaBuilder` carries a module-private `unique
// symbol` brand (see src/types.ts) — importing it from a second,
// independently-compiled declaration site (raw src vs. the package's
// bundled dist) makes it a nominally DIFFERENT type even though the
// runtime class is identical, so `declared instanceof SchemaBuilder`
// below would silently mismatch the class `schema(...)` actually
// constructs, and `Db<T>`'s `SchemaInput` bound would reject those same
// schema() values at the type layer. Importing all three from the
// published entry keeps this helper on the same module instance as
// every test file that calls `schema(...)`.
import { SchemaBuilder, t } from "@zeroship/db";
import type { Db, SchemaInput } from "@zeroship/db";
import type { NativeDb } from "../src/native.js";

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
 * Build the descriptor the toolchain would have emitted for `schemas`.
 *
 * A declaration is either a `SchemaBuilder` — which carries collection
 * options and named indexes alongside its fields — or a bare field record,
 * which carries only fields and takes the installer's defaults.
 *
 * Exported for tests that call `installSchema` directly to exercise install
 * mechanics (reserved names and re-entrancy) rather
 * than Collection behaviour. Those still need a descriptor, or the installer
 * has no collections to apply the mechanics to and the assertions pass
 * vacuously.
 */
export function descriptorFor(schemas: Record<string, unknown>): RuntimeSchemaDescriptor {
  const collections = Object.create(null) as Record<string, unknown>;

  for (const [name, declared] of Object.entries(schemas)) {
    const builder = declared instanceof SchemaBuilder ? declared : null;
    const fields = builder ? builder.fields : declared;
    const options = builder?.options;

    const normalized = normalizeSchema(fields as Parameters<typeof normalizeSchema>[0]);
    const generated = normalizeSchema(generatedSchema);
    if (options?.softDelete) generated.deleted_at.softDelete = true;
    if (options?.versioning) generated.version.concurrency = true;
    collections[name] = {
      // The descriptor carries wire FieldDefs, not `t.*` builders.
      // `normalizeSchema` is the same conversion the installer applies to
      // descriptor fields, so running it here is idempotent downstream.
      fields: { ...generated, ...normalized },
      options: {
        softDelete: options?.softDelete ?? false,
        versioning: options?.versioning ?? false,
        ...(options?.strictness !== undefined ? { strictness: options.strictness } : {}),
      },
      indexes: (builder?.indexes ?? []).map((idx) => ({
        name: idx.name,
        fields: [...idx.fields],
        ...(idx.unique ? { unique: true } : {}),
      })),
    };
  }

  return { version: 2, collections } as RuntimeSchemaDescriptor;
}

export function installSchemaForTest<
  const T extends Record<string, SchemaInput>,
>(
  schemas: T,
  opts: { native: NativeDb; naming?: InstallSchemaOptions["naming"] },
): Db<FixtureSchemas<T>> {
  installSchema(schemas as never, opts.native, {
    descriptor: descriptorFor(schemas),
    ...(opts.naming ? { naming: opts.naming } : {}),
  } as never);
  // The installer plants the per-collection wrappers and the `transaction` /
  // `live` extensions on the native handle as own properties, so the handle
  // itself is the `Db<T>` the call sites (`db.users.find(...)`) expect.
  return opts.native as unknown as Db<FixtureSchemas<T>>;
}
