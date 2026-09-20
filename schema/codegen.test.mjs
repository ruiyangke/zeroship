import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const schemaDir = dirname(fileURLToPath(import.meta.url));

/** Run the generator over a mutated schema and return its combined output. */
function generateFrom(mutate) {
  const scratch = mkdtempSync(join(tmpdir(), "zs-codegen-test-"));
  try {
    const scratchSchema = join(scratch, "schema");
    mkdirSync(scratchSchema, { recursive: true });
    copyFileSync(join(schemaDir, "codegen.mjs"), join(scratchSchema, "codegen.mjs"));

    const schema = JSON.parse(readFileSync(join(schemaDir, "project-v1.json"), "utf8"));
    mutate(schema);
    writeFileSync(
      join(scratchSchema, "project-v1.json"),
      JSON.stringify(schema, null, 2),
    );

    const generated = spawnSync(process.execPath, [join(scratchSchema, "codegen.mjs")], {
      cwd: scratch,
      encoding: "utf8",
    });
    assert.notEqual(generated.status, null, generated.error?.message);
    return { status: generated.status, output: `${generated.stdout}${generated.stderr}` };
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

const UNCOVERED_FIELD_CASES = {
  root: (schema, field) => {
    schema.properties[field] = { type: "boolean" };
  },
  environment: (schema, field) => {
    schema.properties.environments.additionalProperties.properties[field] = {
      type: "boolean",
    };
  },
  // A map ENTRY member is reached only through the map walk, so these two are
  // what prove a field added under a label cannot skip both readers.
  database: (schema, field) => {
    schema.$defs.database.properties[field] = { type: "boolean" };
  },
  app: (schema, field) => {
    schema.$defs.app.properties[field] = { type: "boolean" };
  },
};

for (const [kind, mutate] of Object.entries(UNCOVERED_FIELD_CASES)) {
  test(`codegen rejects an uncovered ${kind} field`, () => {
    const field = `future${kind[0].toUpperCase()}${kind.slice(1)}Flag`;
    const { status, output } = generateFrom((schema) => mutate(schema, field));
    assert.notEqual(status, 0, "generator accepted an uncovered schema field");
    assert.match(output, /reader coverage gap/);
    assert.match(output, new RegExp(field));
  });
}

test("codegen refuses a map that does not say what a label may be", () => {
  const { status, output } = generateFrom((schema) => {
    delete schema.properties.apps.propertyNames;
  });
  assert.notEqual(status, 0, "generator accepted a map with no label rule");
  assert.match(output, /propertyNames/);
});

test("codegen refuses a map that does not name its generated constants", () => {
  const { status, output } = generateFrom((schema) => {
    delete schema.properties.databases["x-entry-name"];
  });
  assert.notEqual(status, 0, "generator accepted a map with no x-entry-name");
  assert.match(output, /x-entry-name/);
});

// The control for every rejection above: a generator that failed on anything
// would pass all five while proving nothing about the mutation each applies.
test("codegen accepts the committed schema", () => {
  const { status, output } = generateFrom(() => {});
  assert.equal(status, 0, output);
});
