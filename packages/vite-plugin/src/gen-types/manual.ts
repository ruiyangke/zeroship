/** Evaluate declared SDK schemas into migration descriptors; policy supplies injected columns. */

import { promises as fs } from "node:fs";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";

import type { FieldDef, NamedIndexSpec, SchemaOptions } from "@zeroship/db";

import type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  IndexDescriptorDto,
  RuntimeOptionsDto,
} from "./addon.js";
import { bundleNodePaths, bundleTmpDir } from "./tmp.js";

/**
 * The structural view of a `@zeroship/db` `SchemaBuilder` the mapper reads.
 * Duck-typed (not `instanceof`): the author's `schema.ts` is bundled with its
 * OWN `@zeroship/db` copy, so the evaluated `SchemaBuilder` is a different module
 * instance than this file's — identity checks would fail. The public surface
 * (`.fields` / `.options` / `.indexes`) is stable, so structural reads are the
 * correct boundary.
 */
interface SchemaBuilderShape {
  fields: Record<string, unknown>;
  options: Readonly<SchemaOptions>;
  indexes: readonly NamedIndexSpec[];
}

/** The owner-app stamp used for the manual/gen-types path (mirrors the generated
 *  path's `app_local`). The fold only consumes ops; ownership never surfaces in
 *  the emitted artifacts, so a stable local stamp is sufficient. */
export const MANUAL_OWNER_APP = "app_local";

/**
 * Evaluate a `schema.ts` module into its declared collection map. Bundles the TS
 * source (aliasing/externalising `@zeroship/db` onto the single installed
 * instance, so `instanceof SchemaBuilder` holds across the boundary), imports it,
 * and returns the `schema` export.
 */
export async function evaluateSchemaModule(
  schemaTsPath: string,
): Promise<Record<string, unknown>> {
  const outFile = join(await bundleTmpDir(), `zs-schema-${randomUUID()}.mjs`);
  try {
    await build({
      entryPoints: [schemaTsPath],
      outfile: outFile,
      bundle: true,
      format: "esm",
      platform: "node",
      target: "node20",
      // `@zeroship/db` is bundled IN (self-contained): the mapper reads the
      // evaluated builders structurally (`.fields`/`.options`/`.indexes` +
      // `.toFieldDef()`), never by class identity, so a private copy is fine.
      nodePaths: bundleNodePaths(),
      logLevel: "silent",
    });
    const mod = (await import(pathToFileURL(outFile).href)) as {
      schema?: unknown;
      default?: unknown;
    };
    const declared = mod.schema ?? mod.default;
    if (declared === null || typeof declared !== "object") {
      throw new Error(
        `gen-types: ${schemaTsPath} must export a \`schema\` object mapping ` +
          `collection names to \`@zeroship/db\` schema(...) builders ` +
          `(named export \`schema\` or default)`,
      );
    }
    return declared as Record<string, unknown>;
  } finally {
    await fs.rm(outFile, { force: true });
  }
}

/**
 * Map an evaluated `schema.ts` collection map → `CollectionDescriptorDto[]`.
 * Each value must be a `@zeroship/db` `SchemaBuilder`.
 */
export function schemaModuleToDescriptors(
  declared: Record<string, unknown>,
): CollectionDescriptorDto[] {
  const descriptors: CollectionDescriptorDto[] = [];
  for (const [name, value] of Object.entries(declared)) {
    const builder = asSchemaBuilder(value);
    if (builder === null) {
      throw new Error(
        `gen-types: schema collection ${JSON.stringify(name)} must be a ` +
          `@zeroship/db schema(...) builder (got ${describe(value)})`,
      );
    }
    descriptors.push(schemaBuilderToDescriptor(name, builder));
  }
  return descriptors;
}

/** Structurally recognise a `SchemaBuilder` (cross-instance safe). */
function asSchemaBuilder(value: unknown): SchemaBuilderShape | null {
  if (value === null || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (
    v.fields === null ||
    typeof v.fields !== "object" ||
    v.options === null ||
    typeof v.options !== "object" ||
    !Array.isArray(v.indexes)
  ) {
    return null;
  }
  return v as unknown as SchemaBuilderShape;
}

/** Map one `SchemaBuilder` (fields + options + indexes) → a `CollectionDescriptorDto`. */
export function schemaBuilderToDescriptor(
  name: string,
  builder: SchemaBuilderShape,
): CollectionDescriptorDto {
  const fieldsMap = builder.fields;
  const fields: FieldDescriptorDto[] = [];
  for (const [fieldName, tb] of Object.entries(fieldsMap)) {
    fields.push(fieldDefToDto(name, fieldName, toFieldDef(name, fieldName, tb)));
  }

  const indexes: IndexDescriptorDto[] = builder.indexes.map((idx) => {
    const dto: IndexDescriptorDto = { name: idx.name, columns: [...idx.fields] };
    if (idx.unique) dto.unique = true;
    return dto;
  });

  const opts = builder.options;
  const runtimeOptions: RuntimeOptionsDto = {
    softDelete: opts.softDelete,
    versioning: opts.versioning,
    strictness: opts.strictness,
  };

  const descriptor: CollectionDescriptorDto = {
    name,
    ownerApp: MANUAL_OWNER_APP,
    fields,
    runtimeOptions,
  };
  if (indexes.length > 0) descriptor.indexes = indexes;
  return descriptor;
}

/** Pull the FieldDef out of a `TypeBuilder` via its `.toFieldDef()` accessor. */
function toFieldDef(
  collection: string,
  fieldName: string,
  tb: unknown,
): FieldDef {
  if (
    tb === null ||
    typeof tb !== "object" ||
    typeof (tb as { toFieldDef?: unknown }).toFieldDef !== "function"
  ) {
    throw new Error(
      `gen-types: collection ${JSON.stringify(collection)} field ` +
        `${JSON.stringify(fieldName)} must be a @zeroship/db t.*() builder ` +
        `(got ${describe(tb)})`,
    );
  }
  return (tb as { toFieldDef(): FieldDef }).toFieldDef();
}

/**
 * The `@zeroship/db` `TypeName` tokens the migrate producer's
 * `token_to_col_type` accepts, mapped from the SDK spelling to the descriptor
 * `type` token. A `TypeName` absent from this table has NO home in
 * `CollectionDescriptorDto` and is rejected (never silently dropped).
 */
const TYPE_TOKEN: Readonly<Record<string, string>> = {
  string: "string",
  number: "number",
  integer: "integer",
  int: "int",
  bigInt: "bigInt",
  boolean: "boolean",
  timestamp: "timestamp",
  json: "json",
  object: "object",
  array: "array",
  bytes: "bytes",
  id: "id",
  ref: "ref",
  vector: "vector",
  geoPoint: "geoPoint",
  char: "char",
  inet: "inet",
};

/**
 * The `@zeroship/db` `TypeName`s the manual mapper cannot yet route through the
 * descriptor producer — each needs a dedicated descriptor facet or a
 * flat-expansion pass that does not exist on this path. Rejected with a precise
 * message so the gap is visible, never a silent drop.
 */
const UNSUPPORTED_TYPE_REASON: Readonly<Record<string, string>> = {
  calendarDate:
    "calendarDate has no descriptor type token (token_to_col_type omits it)",
  actor: "actor has no descriptor type token; declare the underlying id/ref column",
  literal: "top-level literal columns are not modelled by the descriptor producer",
  union:
    "union columns require flat-expansion (normalizeSchema) before the descriptor path",
  uuid:
    "uuid has no descriptor type token (token_to_col_type omits it)",
  double:
    "double has no descriptor type token; the runtime float token is number",
};

/**
 * The one error-code family for every facet the declared-schema path refuses.
 * A shared builder lets a creator write DDL-only facets where no migration
 * executes them; the descriptor emitter refuses rather than silently dropping,
 * and this code is how a caller recognises that refusal.
 */
const SCHEMA_FACET_NOT_DECLARABLE = "SCHEMA_FACET_NOT_DECLARABLE" as const;

function facetNotDeclarable(where: string, facet: string, reason: string): Error {
  return Object.assign(
    new Error(
      `gen-types: field ${where} declares the "${facet}" facet, which is not ` +
        `declarable on the declared-schema path — ${reason}. Author this ` +
        `schema through op.* migrations.`,
    ),
    { code: SCHEMA_FACET_NOT_DECLARABLE, facet },
  );
}

/**
 * Map one `@zeroship/db` `FieldDef` → a `FieldDescriptorDto`. THROWS on any facet
 * with no descriptor home — the correctness-critical no-silent-drop boundary.
 */
export function fieldDefToDto(
  collection: string,
  fieldName: string,
  def: FieldDef,
): FieldDescriptorDto {
  const where = `${collection}.${fieldName}`;

  // Standalone type objects are migration-authored: a declared schema cannot
  // declare the ENUM/DOMAIN it references, so the reference is refused rather
  // than lowered to a type the descriptor producer cannot resolve.
  if (def.type === "enum") {
    throw facetNotDeclarable(where, "enum", "standalone type objects are migration-authored");
  }
  if (def.type === "domain") {
    throw facetNotDeclarable(where, "domain", "standalone type objects are migration-authored");
  }

  if (def.type in UNSUPPORTED_TYPE_REASON) {
    throw new Error(
      `gen-types: field ${where} of type "${def.type}" cannot be mapped to a ` +
        `CollectionDescriptorDto — ${UNSUPPORTED_TYPE_REASON[def.type]}. ` +
        `Author this schema through op.* migrations, or extend the manual mapper.`,
    );
  }
  const token = TYPE_TOKEN[def.type];
  if (token === undefined) {
    throw new Error(
      `gen-types: field ${where} has unknown @zeroship/db type "${def.type}" ` +
        `with no CollectionDescriptorDto home — refusing to silently drop it.`,
    );
  }

  const dto: FieldDescriptorDto = { name: fieldName, type: token };

  // `required` — the DTO models NOT NULL; a non-required field stays absent (the
  // default-nullable `t.*` image), matching `descriptors_to_create_ops`.
  if (def.required) dto.required = true;
  if (def.unique) dto.unique = true;

  if (def.default !== undefined) {
    if (typeof def.default === "function") {
      // A function default (`() => …`) has no static descriptor image; the
      // migrate producer only carries literal defaults. Refuse rather than drop.
      throw facetNotDeclarable(
        where,
        "default",
        "a function default has no static descriptor image; use a literal default",
      );
    }
    dto.default = def.default;
  }

  if (def.clientDefault !== undefined) {
    // An SDK-evaluated default is a live function. The CollectionDescriptorDto
    // crosses a JSON boundary into the engine, where a function cannot survive,
    // so the runtime descriptor would silently lose the default. Refuse rather
    // than drop it; a database default belongs on `.default(value)`.
    throw facetNotDeclarable(
      where,
      "clientDefault",
      "an SDK-evaluated factory cannot cross the descriptor JSON boundary; use .default(value) for a database default",
    );
  }

  // DDL-only facets: identity/generated imply write-input exclusion, a runtime
  // fact the descriptor must drive via assignment metadata, so the declared path
  // refuses rather than substituting a DDL behaviour for it.
  if (def.identity !== undefined) {
    throw facetNotDeclarable(
      where,
      "identity",
      "identity is DDL-authored; use an assignment generator instead",
    );
  }
  if (def.generated !== undefined) {
    throw facetNotDeclarable(
      where,
      "generated",
      "computed columns are DDL-authored; model the value at write time instead",
    );
  }
  // `collation` is purely physical and has no descriptor slot, so it is accepted
  // and ignored: the database applies it to every query, so there is no runtime
  // behaviour to drop. `caseSensitive` is the same physical facet, and the
  // descriptor DOES carry it, so it is accepted and passed through.

  if (def.min !== undefined) dto.min = def.min;
  if (def.max !== undefined) dto.max = def.max;
  if (def.enum !== undefined) dto.enum = [...def.enum];

  // `ref` facets.
  if (def.refTarget !== undefined) dto.references = def.refTarget;
  if (def.refColumn !== undefined) dto.referenceColumn = def.refColumn;
  if (def.refName !== undefined) dto.referenceName = def.refName;
  if (def.relation !== undefined) dto.relation = def.relation;
  if (def.onDelete !== undefined) dto.onDelete = def.onDelete;
  if (def.onUpdate !== undefined) dto.onUpdate = def.onUpdate;
  if (def.deferrable !== undefined) dto.deferrable = def.deferrable;

  // `id` prefix.
  if (def.idPrefix !== undefined) dto.idPrefix = def.idPrefix;

  // Bounded string width: `string` is a two-type token, and without this the
  // descriptor loses the bound and degrades a typed id to unbounded text.
  if (def.maxLength !== undefined) dto.maxLength = def.maxLength;

  // Fixed `CHAR(N)` width.
  if (def.charLength !== undefined) dto.charLen = def.charLength;

  // `vector` facets.
  if (def.vectorDims !== undefined) dto.vectorDims = def.vectorDims;
  if (def.vectorMetric !== undefined) dto.vectorMetric = def.vectorMetric;

  // Physical text-comparison facet; carried so the DDL keeps it.
  if (def.caseSensitive !== undefined) dto.caseSensitive = def.caseSensitive;

  // Encryption + masking (verbatim sub-objects).
  if (def.encrypted !== undefined) dto.encrypted = def.encrypted;
  if (def.mask !== undefined) dto.mask = def.mask;

  // Facets that DO NOT round-trip through the descriptor producer. `index`
  // (single-field `.index()` flag) is subsumed by named indexes on this path;
  // `pattern`, `shape`, `items`, `literalValue`, `variants`, `discriminator`,
  // Assignment generators have no manual descriptor home. Reject rather than
  // silently drop when the author actually used one.
  rejectUnmappableFacet(where, def, "pattern");
  rejectUnmappableFacet(where, def, "shape");
  rejectUnmappableFacet(where, def, "items");
  rejectUnmappableFacet(where, def, "literalValue");
  rejectUnmappableFacet(where, def, "variants");
  rejectUnmappableFacet(where, def, "discriminator");
  rejectUnmappableFacet(where, def, "assign");
  rejectUnmappableFacet(where, def, "enumName");
  rejectUnmappableFacet(where, def, "enumSchema");
  rejectUnmappableFacet(where, def, "domainName");
  rejectUnmappableFacet(where, def, "domainSchema");
  rejectUnmappableFacet(where, def, "arrayStorage");

  return dto;
}

/** Throw if a FieldDef carries a facet the descriptor producer has no home for. */
function rejectUnmappableFacet(
  where: string,
  def: FieldDef,
  facet: keyof FieldDef,
): void {
  if (def[facet] !== undefined) {
    throw facetNotDeclarable(where, String(facet), "it has no CollectionDescriptorDto home");
  }
}

function describe(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}
