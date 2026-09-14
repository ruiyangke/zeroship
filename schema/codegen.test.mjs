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

for (const kind of ["root", "environment"]) {
  test(`codegen rejects an uncovered ${kind} field`, () => {
    const scratch = mkdtempSync(join(tmpdir(), "zs-codegen-test-"));
    try {
      const scratchSchema = join(scratch, "schema");
      mkdirSync(scratchSchema, { recursive: true });
      copyFileSync(join(schemaDir, "codegen.mjs"), join(scratchSchema, "codegen.mjs"));

      const schema = JSON.parse(readFileSync(join(schemaDir, "project-v1.json"), "utf8"));
      const field = kind === "root" ? "futureRootFlag" : "futureEnvironmentFlag";
      if (kind === "root") {
        schema.properties[field] = { type: "boolean" };
      } else {
        schema.properties.environments.additionalProperties.properties[field] = {
          type: "boolean",
        };
      }
      writeFileSync(
        join(scratchSchema, "project-v1.json"),
        JSON.stringify(schema, null, 2),
      );

      const generated = spawnSync(process.execPath, [join(scratchSchema, "codegen.mjs")], {
        cwd: scratch,
        encoding: "utf8",
      });
      assert.notEqual(generated.status, null, generated.error?.message);
      assert.notEqual(generated.status, 0, "generator accepted an uncovered schema field");
      const output = `${generated.stdout}${generated.stderr}`;
      assert.match(output, /reader coverage gap/);
      assert.match(output, new RegExp(field));
    } finally {
      rmSync(scratch, { recursive: true, force: true });
    }
  });
}
