// `genArtifacts` hands a JS caller the folded schema as STRUCTURE, not only as the
// two artifact strings.
//
// `env.db.ts` and `schema.runtime.json` are consumed downstream by a platform that
// wants to render its own files. Before this reply carried `collections`, the only
// route to the metadata was to JSON.parse `runtimeJson` — the string the addon had
// just serialized from the very structure the caller wanted back.
//
// This arm exists because the Rust suites cannot reach the question it asks. They run
// on the napi-free `--no-default-features` build, where there is no `.node`, no
// `index.d.ts` and no JS object: a field can be present in the Rust reply struct and
// still fail to cross, and the two are only the same claim once something loads the
// real addon and reads the property. That is what this does.
//
// DB-FREE on purpose. `genArtifacts` touches no database, so this arm runs on every
// host invocation rather than skipping when a server URL is unset — which also makes
// it the export's only unconditional coverage.
//
// NOT covered here: that the exported values are CORRECT. That is the fold's claim and
// is adjudicated against live servers elsewhere (`gen-artifacts-dialect.test.ts` reads
// a real catalog back). This arm asserts the export CROSSES and agrees with the
// artifact emitted beside it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

import { table, t } from "zero-migrate";
import { buildEnvelope } from "zero-migrate/internal/recorder";
import { currentIrVersion } from "zero-migrate-cli";
import { noInjectPolicy } from "./policy.js";

// The host suite's addon is resolved and freshness-checked in one place.
import "./addon.js";

interface FieldDescriptorDto {
  name: string;
  type: string;
  required?: boolean;
  unique?: boolean;
  maxLength?: number;
  references?: string;
  referenceColumn?: string;
  referenceName?: string;
  onDelete?: string;
}

interface CollectionDescriptorDto {
  name: string;
  ownerApp: string;
  fields: FieldDescriptorDto[];
  indexes?: { name: string; columns: string[]; unique?: boolean }[];
  runtimeOptions?: { softDelete?: boolean; versioning?: boolean; strictness?: string };
}

interface GenArtifactsReply {
  ok: boolean;
  envDbTs?: string;
  runtimeJson?: string;
  error?: string;
  hasDialectalOps?: boolean;
  collections?: CollectionDescriptorDto[];
  dialect?: string;
}

interface GenArtifactsAddon {
  genArtifacts(source: {
    envelopes?: unknown[];
    projectSchema?: string;
    charterLayers: string[];
    dialect: string;
  }): GenArtifactsReply;
}

// The raw `.node` is loaded directly: `zero-migrate-cli` re-exports the apply/status
// verbs but not the DB-free artifact emitter, so there is no CLI seam to ride here.
const addon = createRequire(import.meta.url)(
  process.env.ZERO_MIGRATE_ADDON_PATH as string,
) as GenArtifactsAddon;

const SCHEMA = "public";

/** A table whose columns exercise the facets a downstream renderer needs and the wire
 *  historically dropped: a bounded `VARCHAR(n)` width and a typed reference carrying
 *  an explicit target column plus an ON DELETE action. */
const migration = {
  name: "export_shapes",
  schema() {
    table("authors").create({ columns: { name: t.text() } });
    table("posts").create({
      columns: {
        slug: t.string({ length: 40 }),
        body: t.text(),
        author_id: t
          .text()
          .references("authors", "id", { onDelete: "cascade", name: "posts_author_fk" }),
      },
    });
  },
};

function generate(target: string): GenArtifactsReply {
  return addon.genArtifacts({
    envelopes: [buildEnvelope(migration, { irVersion: currentIrVersion() })],
    projectSchema: SCHEMA,
    charterLayers: [noInjectPolicy(SCHEMA)],
    dialect: target,
  });
}

test("genArtifacts exports the folded collections to a JS caller", () => {
  const reply = generate("postgres");
  assert.ok(reply.ok, `genArtifacts ok: ${reply.error}`);

  const collections = reply.collections;
  assert.ok(Array.isArray(collections), "the reply carries a collections array");
  assert.deepEqual(
    collections.map((c) => c.name),
    ["authors", "posts"],
    "collections are exported in name order",
  );

  const posts = collections.find((c) => c.name === "posts");
  assert.ok(posts, "the posts collection is exported");

  // The export agrees with the artifact emitted beside it — one fold, one recovery.
  const parsed = JSON.parse(reply.runtimeJson as string) as {
    collections: Record<string, { fields: Record<string, unknown> }>;
  };
  assert.deepEqual(
    posts.fields.map((f) => f.name).sort(),
    Object.keys(parsed.collections.posts.fields).sort(),
    "the exported field set equals the one schema.runtime.json serializes",
  );

  // The VARCHAR width, which had no wire slot until this change and which a
  // downstream renderer needs to emit `VARCHAR(40)` rather than `TEXT`.
  const slug = posts.fields.find((f) => f.name === "slug");
  assert.equal(slug?.maxLength, 40, "the bounded string width crosses to JS");

  // The reference identity a `ref` brand alone cannot express.
  const author = posts.fields.find((f) => f.name === "author_id");
  assert.equal(author?.references, "authors");
  assert.equal(author?.referenceColumn, "id");
  assert.equal(author?.referenceName, "posts_author_fk");
  assert.equal(author?.onDelete, "cascade");
});

test("genArtifacts names the dialect it folded under, in the payload", () => {
  for (const target of ["postgres", "mysql", "sqlite"]) {
    const reply = generate(target);
    assert.ok(reply.ok, `${target}: ${reply.error}`);
    // Leg selection changes WHICH COLUMNS EXIST, so an export is uninterpretable
    // without the target it was folded under; a caller must not have to remember
    // what it sent.
    assert.equal(reply.dialect, target, `${target}: the reply names its own target`);
  }
});

test("a refused genArtifacts call exports neither collections nor a dialect", () => {
  const reply = addon.genArtifacts({
    envelopes: [buildEnvelope(migration, { irVersion: currentIrVersion() })],
    projectSchema: SCHEMA,
    charterLayers: [noInjectPolicy(SCHEMA)],
    dialect: "duckdb",
  });
  assert.equal(reply.ok, false, "an unknown dialect is refused");
  // `undefined`, not `[]` and not the rejected input string. A consumer testing
  // falsiness would conflate a refusal with an empty schema, which is why the
  // producing side must never emit one for the other.
  assert.equal(reply.collections, undefined, "a refusal reports no collections");
  assert.equal(reply.dialect, undefined, "a refusal names no dialect");
});
