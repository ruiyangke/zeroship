import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(packageRoot, "../..");
const createBin = join(packageRoot, "bin/create.js");
const vitePluginRoot = join(repoRoot, "packages/vite-plugin");
const configDump = join(vitePluginRoot, "scripts/project-config-dump.ts");
const allowedShape =
  "use 1-63 lowercase letters, digits, or hyphens; start with a letter or digit";

function scaffold(cwd, name) {
  return spawnSync(process.execPath, [createBin, name], {
    cwd,
    encoding: "utf8",
  });
}

function assertRefused(name) {
  const root = mkdtempSync(join(tmpdir(), "zs-create-name-"));
  try {
    const result = scaffold(root, name);
    assert.notEqual(result.status, 0, `${name} must be refused`);
    assert.match(result.stderr, new RegExp(allowedShape));
    assert.equal(existsSync(join(root, name)), false, "a refused name must create nothing");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("refuses an underscore that the project schema cannot parse", () => {
  assertRefused("my_app");
});

test("refuses a 64-character name that the project schema cannot parse", () => {
  assertRefused("a".repeat(64));
});

test("the longest legal scaffold emits a project config the schema reader accepts", () => {
  const root = mkdtempSync(join(tmpdir(), "zs-create-name-"));
  const name = "a".repeat(63);
  try {
    const created = scaffold(root, name);
    assert.equal(created.status, 0, created.stderr);

    const configPath = join(root, name, "zeroship.jsonc");
    const parsed = spawnSync(
      process.execPath,
      ["--import", "tsx", configDump, configPath],
      { cwd: vitePluginRoot, encoding: "utf8" },
    );
    assert.equal(parsed.status, 0, parsed.stderr);
    assert.equal(JSON.parse(parsed.stdout).name, name);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("stamps runtime_date from the day the project is scaffolded", () => {
  const root = mkdtempSync(join(tmpdir(), "zs-create-date-"));
  const clock = join(root, "fixed-clock.mjs");
  writeFileSync(
    clock,
    `const RealDate = globalThis.Date;
globalThis.Date = class FixedDate extends RealDate {
  constructor(...args) {
    super(...(args.length === 0 ? ["2042-03-04T12:00:00.000Z"] : args));
  }
  static now() { return new RealDate("2042-03-04T12:00:00.000Z").getTime(); }
};
`,
  );

  try {
    const created = spawnSync(
      process.execPath,
      ["--import", clock, createBin, "dated-project"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(created.status, 0, created.stderr);

    const configPath = join(root, "dated-project", "zeroship.jsonc");
    const parsed = spawnSync(
      process.execPath,
      ["--import", "tsx", configDump, configPath],
      { cwd: vitePluginRoot, encoding: "utf8" },
    );
    assert.equal(parsed.status, 0, parsed.stderr);
    assert.equal(JSON.parse(parsed.stdout).runtime_date, "2042-03-04");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
