/**
 * Prove that schema/codegen.mjs cannot bless a schema leaf that either reader
 * does not cover. The mutation runs in a temporary repository-shaped tree and
 * never edits the checkout.
 *
 * Prints BLESSED when generate + --check both accept the uncovered field,
 * REJECTED when the generator names the intended coverage gap, and exits 2 for
 * any unrelated setup failure.
 */

import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "..", "..");
const kind = process.argv[2];
const field = kind === "root" ? "futureRootFlag" : "futureEnvironmentFlag";
if (kind !== "root" && kind !== "environment") {
  console.error("usage: project_config_codegen_probe.mjs root|environment");
  process.exit(2);
}

const scratch = mkdtempSync(join(tmpdir(), "zs-codegen-probe-"));
try {
  const schemaDir = join(scratch, "schema");
  mkdirSync(schemaDir, { recursive: true });
  copyFileSync(join(repo, "schema", "codegen.mjs"), join(schemaDir, "codegen.mjs"));

  const schema = JSON.parse(readFileSync(join(repo, "schema", "project-v1.json"), "utf8"));
  if (kind === "root") {
    schema.properties[field] = { type: "boolean" };
  } else {
    schema.properties.environments.additionalProperties.properties[field] = { type: "boolean" };
  }
  writeFileSync(join(schemaDir, "project-v1.json"), JSON.stringify(schema, null, 2));

  const run = (...args) => spawnSync(process.execPath, [join(schemaDir, "codegen.mjs"), ...args], {
    cwd: scratch,
    encoding: "utf8",
  });
  const generated = run();
  if (generated.status !== 0) {
    const output = `${generated.stdout}${generated.stderr}`;
    if (output.includes("reader coverage gap") && output.includes(field)) {
      process.stdout.write("REJECTED\n");
    } else {
      process.stderr.write(output);
      process.exitCode = 2;
    }
  } else {
    const checked = run("--check");
    if (checked.status === 0) {
      process.stdout.write("BLESSED\n");
    } else {
      const output = `${checked.stdout}${checked.stderr}`;
      if (output.includes("reader coverage gap") && output.includes(field)) {
        process.stdout.write("REJECTED\n");
      } else {
        process.stderr.write(output);
        process.exitCode = 2;
      }
    }
  }
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
