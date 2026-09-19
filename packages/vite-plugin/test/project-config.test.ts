/**
 * The TypeScript reader for `zeroship.jsonc`.
 *
 * WHAT THESE DO NOT CATCH, so their greenness is not overread:
 *
 * - They do not run the Rust reader in process. Both readers consume generated
 *   contracts from the shared schema, and each owning suite exercises the
 *   committed cross-tool fixture.
 * - They do not run a real `vite build`, so nothing here proves the plugin
 *   actually threads the resolved config into the packer. That is an
 *   end-to-end check's job.
 * - The escape-hatch tests prove the deny-list REFUSES; they do not prove the
 *   deny-list is complete, because completeness is a property of the schema's
 *   `x-cli-read` markers, not of this file. `deny_list_is_generated_not_typed`
 *   below is the closest thing, and it only checks the list is non-trivial and
 *   contains the fields the CLI is known to read.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
  applyProjectConfigOverride,
  canonicalJson,
  CONFIG_ENV_VAR,
  CONFIG_FILENAME,
  CLI_READ_FIELDS,
  createProjectConfigHolder,
  defaultProjectConfig,
  loadProjectConfig,
  locateProjectConfig,
  parseProjectConfig,
  readProjectConfig,
  resolveProjectConfig,
} from "../src/project-config/index.js";

const FULL = `{
  // a comment, which is the whole reason the format is JSONC
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {
    "main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship" }
  },
  "apps": {
    "storefront": { "app": "11111111-1111-4111-8111-111111111111", "databases": ["main"], "primary": "main" }
  },
  "secrets": ["STRIPE_SECRET_KEY"],
  "environments": {
    "staging": {
      "control": "https://control.staging.zeroship.ai",
      "apps": { "storefront": { "app": "22222222-2222-4222-8222-222222222222" } },
      "databases": { "main": { "id": "dbs_03evr3oqx1200uzh8k6gycpgg" } },
      "protected": true
    }
  }
}`;

function scratch(files: Record<string, string> = {}): string {
  const root = mkdtempSync(join(tmpdir(), "zs-projcfg-"));
  for (const [rel, body] of Object.entries(files)) {
    const abs = join(root, rel);
    mkdirSync(join(abs, ".."), { recursive: true });
    writeFileSync(abs, body);
  }
  return root;
}

function parsed(body = FULL) {
  return parseProjectConfig("zeroship.jsonc", body);
}

function schemaCliReadFields(): string[] {
  const schema = JSON.parse(
    readFileSync(resolve(import.meta.dirname, "..", "..", "..", "schema", "project-v1.json"), "utf8"),
  ) as Record<string, any>;
  const fields = new Set<string>();
  const deref = (node: Record<string, any>) => {
    if (node.$ref == null) return node;
    const name = String(node.$ref).split("/").at(-1)!;
    const { $ref: _drop, ...siblings } = node;
    return { ...schema.$defs[name], ...siblings };
  };
  const visit = (raw: Record<string, any>, path: string) => {
    const node = deref(raw);
    if (node["x-cli-read"] === true) fields.add(path);
    if (node.type !== "object") return;
    // A map keys its values by a creator-chosen LABEL, so its members take a
    // `*` segment. Walking only `properties` would miss every one of them and
    // let this independent re-derivation agree with a generator that had
    // dropped them.
    const entry =
      node.additionalProperties != null && typeof node.additionalProperties === "object"
        ? deref(node.additionalProperties as Record<string, any>)
        : null;
    if (entry != null) {
      for (const [key, child] of Object.entries(entry.properties ?? {})) {
        visit(child as Record<string, any>, `${path}.*.${key}`);
      }
      return;
    }
    for (const [key, child] of Object.entries(node.properties ?? {})) {
      visit(child as Record<string, any>, path ? `${path}.${key}` : key);
    }
  };
  for (const [key, child] of Object.entries(schema.properties)) {
    if (key !== "environments") visit(child as Record<string, any>, key);
  }
  for (const [key, child] of Object.entries(
    schema.properties.environments.additionalProperties.properties,
  )) {
    visit(child as Record<string, any>, key);
  }
  return [...fields].sort();
}

/**
 * Resolve a dotted path against the fixture, taking a `*` segment to mean the
 * first label present. The fixture declares one entry per map, so the choice
 * is unambiguous there.
 */
function segments(value: Record<string, any>, dotted: string): string[] {
  const out: string[] = [];
  let cursor: any = value;
  for (const part of dotted.split(".")) {
    const key = part === "*" ? Object.keys(cursor ?? {})[0]! : part;
    out.push(key);
    cursor = cursor?.[key];
  }
  return out;
}

function valueAt(value: Record<string, any>, dotted: string): unknown {
  return segments(value, dotted).reduce((cursor, part) => cursor?.[part], value as any);
}

function setValueAt(value: Record<string, any>, dotted: string, replacement: unknown): void {
  const parts = segments(value, dotted);
  const member = parts.pop()!;
  const parent = parts.reduce((cursor, part) => cursor[part], value);
  parent[member] = replacement;
}

describe("locating the file", () => {
  test("auto-discovery finds it in the app root, and nowhere else", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    try {
      assert.equal(locateProjectConfig(root), join(root, CONFIG_FILENAME));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // NO UPWARD WALK. A build run in a subdirectory must not silently pick up a
  // sibling app's `app` and `control` - the same cross-targeting the
  // environments rule closes, arriving through the file-location door.
  test("a file in the PARENT directory is not found", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    try {
      const sub = join(root, "packages", "web");
      mkdirSync(sub, { recursive: true });
      assert.equal(locateProjectConfig(sub), null);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  // The asymmetry that keeps `zeroship()` working in a scratch directory: only
  // AUTO-discovery may come up empty. A path somebody typed must exist.
  test("an explicitly named file that does not exist throws; auto-discovery returns null", () => {
    const root = scratch();
    try {
      assert.equal(locateProjectConfig(root), null);
      assert.throws(() => locateProjectConfig(root, "nope.jsonc"), /does not exist/);
      process.env[CONFIG_ENV_VAR] = "also-nope.jsonc";
      try {
        assert.throws(() => locateProjectConfig(root), /does not exist/);
      } finally {
        delete process.env[CONFIG_ENV_VAR];
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("configPath wins over the environment variable", () => {
    const root = scratch({ "a.jsonc": FULL, "b.jsonc": FULL });
    try {
      process.env[CONFIG_ENV_VAR] = "b.jsonc";
      try {
        assert.equal(locateProjectConfig(root, "a.jsonc"), join(root, "a.jsonc"));
        assert.equal(locateProjectConfig(root), join(root, "b.jsonc"));
      } finally {
        delete process.env[CONFIG_ENV_VAR];
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("an external config roots its relative paths at the config directory", () => {
    const body = FULL.replace(
      '"mode": "full"',
      '"mode": "full", "serverEntry": "src/server.ts"',
    );
    const root = scratch({ "apps/foo/zeroship.jsonc": body });
    const appRoot = join(root, "apps", "foo");
    try {
      const { config } = readProjectConfig(root, {
        configPath: "apps/foo/zeroship.jsonc",
      });
      assert.equal(config.build.serverEntry, join(appRoot, "src/server.ts"));
      assert.equal(config.build.dist, join(appRoot, "dist"));
      assert.equal(config.build.output, join(appRoot, "dist/app.zship"));
      assert.equal(config.databases!.main.migrations, join(appRoot, "migrations"));
      assert.equal(config.databases!.main.out, join(appRoot, "generated/zeroship"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("external config containment is checked at the config directory", () => {
    const body = FULL.replace('"dist": "dist"', '"dist": "linked-root"');
    const root = scratch({ "apps/foo/zeroship.jsonc": body });
    const appRoot = join(root, "apps", "foo");
    symlinkSync(".", join(appRoot, "linked-root"));
    try {
      assert.throws(
        () => readProjectConfig(root, { configPath: "apps/foo/zeroship.jsonc" }),
        /build\.dist.*zeroship\.jsonc/,
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("validation", () => {
  test("a secret-shaped key anywhere is refused, naming where the value belongs", () => {
    for (const body of [
      '{"name":"a","control":"u","runtime_date":"2026-08-14","password":"x","build":{"mode":"full","dist":"d","output":"o"},"migrations":{"dir":"m","out":"g"}}',
      '{"name":"a","control":"u","runtime_date":"2026-08-14","build":{"mode":"full","dist":"d","output":"o","token":"x"},"migrations":{"dir":"m","out":"g"}}',
    ]) {
      assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /zeroship secret set/);
    }
  });

  // `apps`, `control` and `databases` are all non-inheritable, and each map
  // must cover every root label: an environment that names a control and
  // inherits the root app targets the wrong code, and one that inherits a
  // database id lands WRITES in the wrong data.
  test("an environment missing any of apps, control or databases is refused", () => {
    for (const removed of [
      '"apps": { "storefront": { "app": "22222222-2222-4222-8222-222222222222" } },\n      ',
      '"databases": { "main": { "id": "dbs_03evr3oqx1200uzh8k6gycpgg" } },\n      ',
      '"control": "https://control.staging.zeroship.ai",\n      ',
    ]) {
      const body = FULL.replace(removed, "");
      assert.notEqual(body, FULL, "the fixture must actually change");
      assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /NON-INHERITABLE/);
    }
    // The control: untouched, the same fixture parses.
    parseProjectConfig("zeroship.jsonc", FULL);
  });

  test("an environment that leaves a root label out of a map is refused", () => {
    const two = FULL.replace(
      '"main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship" }',
      '"main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship" },\n    "events": { "id": "dbs_03evr3oqx1200qyvgmdnjrsla", "migrations": "migrations/events", "out": "generated/events" }',
    );
    assert.notEqual(two, FULL, "the second database must actually be added");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", two), /does not name `events`/);

    const covered = two.replace(
      '"databases": { "main": { "id": "dbs_03evr3oqx1200uzh8k6gycpgg" } }',
      '"databases": { "main": { "id": "dbs_03evr3oqx1200uzh8k6gycpgg" }, "events": { "id": "dbs_03evr3oqx12012zcpsh30ivwy" } }',
    );
    parseProjectConfig("zeroship.jsonc", covered);
  });

  test("an app naming a database the file does not declare is refused", () => {
    const body = FULL.replace('"databases": ["main"]', '"databases": ["main", "ghost"]');
    assert.notEqual(body, FULL, "the stray label must actually be added");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /does not\s+declare under/);
  });

  test("an app that uses a database but names no primary is refused", () => {
    const body = FULL.replace(', "primary": "main" }', " }");
    assert.notEqual(body, FULL, "the primary must actually be removed");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /names no `primary`/);
  });

  test("a label that is not a usable member name is refused", () => {
    const body = FULL.replace('"main": { "id": "dbs_', '"__proto__": { "id": "dbs_');
    assert.notEqual(body, FULL, "the label must actually change");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /not a usable label|__proto__/);
  });

  test("secrets entries must look like NAMES, not values", () => {
    const body = FULL.replace('"STRIPE_SECRET_KEY"', '"sk_test_deadbeef"');
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /holds NAMES/);
  });

  test("an unknown top-level key names the known ones", () => {
    const body = FULL.replace('"name": "demo-app",', '"name": "demo-app", "rpcEndpoint": "/_rpc",');
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /rpcEndpoint/);
  });

  // Symmetry with the Rust reader, which refuses the same thing. One tool
  // loading a file the other rejects is the divergence class this file removes.
  test("a foreign $schema id is refused", () => {
    const body = FULL.replace("project-v1.json", "project-v9.json");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /project-v9/);
  });

  test("a bad runtime_date is refused", () => {
    assert.throws(
      () => parseProjectConfig("zeroship.jsonc", FULL.replace('"2026-08-14"', '"August 2026"')),
      /runtime_date/,
    );
  });

  test("build.dist cannot be the project root or one of its ancestors", () => {
    for (const dist of [".", "..", "../..", "dist/..", "/tmp"]) {
      const body = FULL.replace('"dist": "dist"', `"dist": ${JSON.stringify(dist)}`);
      const root = scratch({ [CONFIG_FILENAME]: body });
      try {
        assert.throws(
          () => readProjectConfig(root),
          /build\.dist.*zeroship\.jsonc/,
          `build.dist=${JSON.stringify(dist)} must be refused`,
        );
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    }
  });

  test("build.output cannot target the project root, an ancestor, or an existing source file", () => {
    const cases = [
      { output: ".", files: {} },
      { output: "..", files: {} },
      { output: CONFIG_FILENAME, files: {} },
      { output: "src/main.ts", files: { "src/main.ts": "export default {};\n" } },
    ];
    for (const { output, files } of cases) {
      const body = FULL.replace('"output": "dist/app.zship"', `"output": ${JSON.stringify(output)}`);
      const root = scratch({ [CONFIG_FILENAME]: body, ...files });
      try {
        assert.throws(
          () => readProjectConfig(root),
          /build\.output/,
          `build.output=${JSON.stringify(output)} must be refused`,
        );
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    }
  });

  test("build.output may replace an existing generated artifact", () => {
    const root = scratch({
      [CONFIG_FILENAME]: FULL,
      "dist/app.zship": "old artifact",
    });
    try {
      assert.equal(readProjectConfig(root).config.build.output, join(root, "dist/app.zship"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("build.output rejects a dangling symlink", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    mkdirSync(join(root, "dist"), { recursive: true });
    symlinkSync("../creator-source.ts", join(root, "dist/app.zship"));
    try {
      assert.throws(() => readProjectConfig(root), /build\.output.*non-artifact/);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a database's out cannot target the project root or one of its ancestors", () => {
    for (const out of [".", "..", "generated/zeroship/../..", "/tmp"]) {
      const body = FULL.replace(
        '"out": "generated/zeroship"',
        `"out": ${JSON.stringify(out)}`,
      );
      const root = scratch({ [CONFIG_FILENAME]: body });
      try {
        assert.throws(
          () => readProjectConfig(root),
          /databases\.main\.out/,
          `databases.main.out=${JSON.stringify(out)} must be refused`,
        );
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    }
  });
});

describe("resolution", () => {
  test("the root resolution drops $schema and environments", () => {
    const r = resolveProjectConfig(parsed());
    assert.equal(r.apps!.storefront.app, "11111111-1111-4111-8111-111111111111");
    assert.equal(r.databases!.main.out, "generated/zeroship");
    assert.ok(!("environments" in (r as unknown as Record<string, unknown>)));
    assert.ok(!("$schema" in (r as unknown as Record<string, unknown>)));
  });

  test("an environment replaces control and overrides the id under each label", () => {
    const r = resolveProjectConfig(parsed(), "staging");
    assert.equal(r.apps!.storefront.app, "22222222-2222-4222-8222-222222222222");
    assert.equal(r.control, "https://control.staging.zeroship.ai");
    assert.equal(r.databases!.main.id, "dbs_03evr3oqx1200uzh8k6gycpgg");
    // The label, the build-time paths and the app wiring are the same artifact
    // across environments: only the ids move.
    assert.equal(r.databases!.main.migrations, "migrations");
    assert.equal(r.databases!.main.out, "generated/zeroship");
    assert.deepEqual(r.apps!.storefront.databases, ["main"]);
    assert.equal(r.apps!.storefront.primary, "main");
    assert.equal(r.protected, true);
  });

  test("an unknown environment lists the ones that exist", () => {
    assert.throws(() => resolveProjectConfig(parsed(), "stagng"), /staging/);
  });

  // THE SIDE THAT HAS DEFAULTS. The plugin must work with `zeroship()` and no
  // file at all - that is what the scaffold's vite.config.ts does.
  test("with no file at all, every schema default is applied", () => {
    const root = scratch();
    try {
      const { config, path } = readProjectConfig(root);
      assert.equal(path, null);
      assert.equal(config.build.mode, "full");
      assert.equal(config.build.dist, "dist");
      assert.equal(config.build.output, "dist/app.zship");
      assert.equal(config.databases, undefined, "a scratch directory declares none");
      assert.equal(config.apps, undefined);
      assert.deepEqual(config.secrets, []);
      assert.deepEqual(defaultProjectConfig(), config);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the holder reads the file once per root", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    try {
      const holder = createProjectConfigHolder({});
      const a = holder.load(root);
      // Corrupt the file AFTER the first read. A second load that re-parsed
      // would throw; the point of memoising is that the build and the dev
      // server cannot end up with two different answers mid-run.
      writeFileSync(join(root, CONFIG_FILENAME), "{ not json");
      const b = holder.load(root);
      assert.equal(a, b);
      assert.equal(holder.path(), join(root, CONFIG_FILENAME));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("the config escape hatch", () => {
  test("an external config roots a relative build path override before returning it", () => {
    const root = scratch({ "apps/foo/zeroship.jsonc": FULL });
    const appRoot = join(root, "apps", "foo");
    try {
      const { config } = readProjectConfig(root, {
        configPath: "apps/foo/zeroship.jsonc",
        override: (current) => ({
          build: { ...current.build, dist: "apps/foo" },
        }),
      });
      assert.equal(config.build.dist, join(appRoot, "apps/foo"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the reader rejects a callback that moves build.dist over the project", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    try {
      assert.throws(
        () => readProjectConfig(root, {
          override: (config) => ({ build: { ...config.build, dist: "." } }),
        }),
        /build\.dist.*zeroship\.jsonc/,
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the reader rejects a callback that symlinks build.dist over the project", () => {
    const root = scratch({ [CONFIG_FILENAME]: FULL });
    symlinkSync(".", join(root, "linked-root"));
    try {
      assert.throws(
        () => readProjectConfig(root, {
          override: (config) => ({ build: { ...config.build, dist: "linked-root" } }),
        }),
        /build\.dist.*zeroship\.jsonc/,
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a field only the build reads is overridable", () => {
    const base = resolveProjectConfig(parsed());
    const out = applyProjectConfigOverride(base, (c) => ({
      ...c,
      build: { ...c.build, mode: "static" as const },
    }));
    assert.equal(out.build.mode, "static");
    assert.equal(out.build.output, "dist/app.zship", "the rest of build survives");
  });

  // The documented idiom spreads the WHOLE config, so `app` and `control` are
  // present in the result of every override anybody writes. Denial has to be on
  // CHANGE, not on presence, or the only form in the docs would be rejected.
  test("the spread idiom does not trip the deny-list", () => {
    const base = resolveProjectConfig(parsed());
    const out = applyProjectConfigOverride(base, (c) => ({ ...c }));
    assert.equal(out.apps!.storefront.app, base.apps!.storefront.app);
    assert.equal(out.control, base.control);
  });

  test("every schema-marked CLI field rejects callback mutation without mutating the original", () => {
    for (const field of schemaCliReadFields()) {
      const base = resolveProjectConfig(parsed(), "staging");
      const before = canonicalJson(base);
      const current = valueAt(base as unknown as Record<string, any>, field);
      assert.notEqual(current, undefined, `${field} needs a stated fixture value`);
      const replacement = Array.isArray(current)
        ? [...current, "changed"]
        : typeof current === "boolean"
          ? !current
          : `${String(current)}-changed`;
      const escaped = field.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      assert.throws(
        () => applyProjectConfigOverride(base, (c) => {
          setValueAt(c as unknown as Record<string, any>, field, replacement);
          return {};
        }),
        new RegExp(`may not change \\\`${escaped}\\\``),
        `${field} must be refused after an in-place change`,
      );
      assert.equal(canonicalJson(base), before, `${field} mutation escaped the callback copy`);
    }
  });

  test("the callback receives a deep copy for an accepted build-only mutation", () => {
    const base = resolveProjectConfig(parsed());
    const out = applyProjectConfigOverride(base, (copy) => {
      copy.build.mode = "static";
      return {};
    });
    assert.equal(base.build.mode, "full", "the callback reached the caller's object");
    assert.equal(out.build.mode, "static", "the accepted in-place mutation must survive");
  });

  test("the deny-list and schema markers match the explicit Rust-read contract", () => {
    const expected = [
      "apps.*.app",
      "apps.*.databases",
      "apps.*.primary",
      "build.output",
      "control",
      "databases.*.id",
      "databases.*.migrations",
      "databases.*.out",
      "name",
      "protected",
      "runtime_date",
      "secrets",
    ];
    assert.deepEqual(schemaCliReadFields(), expected);
    assert.deepEqual([...CLI_READ_FIELDS].sort(), expected);
  });
});

describe("the canonical dump", () => {
  test("keys are sorted and the output is compact", () => {
    const dump = canonicalJson(resolveProjectConfig(parsed()));
    assert.ok(dump.startsWith('{"apps":'), dump);
    assert.ok(!dump.includes("\n"), dump);
    assert.equal(canonicalJson({ b: 1, a: 2 }), '{"a":2,"b":1}');
    assert.equal(canonicalJson({ a: 2, b: 1 }), '{"a":2,"b":1}');
  });
});

describe("JSONC edge cases the two readers must agree on", () => {
  test("a // inside a string is data, not a comment", () => {
    const c = parsed().raw as Record<string, unknown>;
    assert.equal(c.control, "https://control.zeroship.ai");
  });

  test("escaped quotes, commas before a brace, trailing commas and multi-byte survive", () => {
    const body =
      '{"name":"a","control":"u","runtime_date":"2026-08-14",' +
      '"build":{"mode":"full","dist":"d","output":"o","serverEntry":"x\\"/*y*/,}-café-😀.ts",},' +
      '"databases":{"main":{"id":"dbs_03evr3oqx1200yyd6zj2cebfw","migrations":"m","out":"g",},},' +
      '"apps":{"storefront":{"databases":["main",],"primary":"main",},},}';
    const r = resolveProjectConfig(parseProjectConfig("zeroship.jsonc", body));
    assert.equal(r.build.serverEntry, 'x"/*y*/,}-café-😀.ts');
  });

  test("CRLF input parses without changing resolved values", () => {
    const r = resolveProjectConfig(parsed(FULL.replaceAll("\n", "\r\n")));
    assert.equal(r.name, "demo-app");
  });

  test("bare carriage return line endings are rejected", () => {
    assert.throws(() => parsed(FULL.replaceAll("\n", "\r")), /bare carriage return/);
  });

  test("Unicode-escaped keys and values are decoded", () => {
    const body = FULL
      .replace('"name": "demo-app"', '"\\u006eame": "demo-app"')
      .replace(
        '"mode": "full"',
        '"mode": "full", "server\\u0045ntry": "caf\\u00e9-\\ud83d\\ude00.ts"',
      );
    const r = resolveProjectConfig(parsed(body));
    assert.equal(r.name, "demo-app");
    assert.equal(r.build.serverEntry, "caf\u00e9-\ud83d\ude00.ts");
  });

  test("an unpaired surrogate is rejected", () => {
    const body = FULL.replace(
      '"mode": "full"',
      '"mode": "full", "serverEntry": "src/\\ud800.ts"',
    );
    assert.throws(() => parsed(body), /unpaired surrogate/);
  });

  test("a leading BOM is rejected", () => {
    assert.throws(() => parsed(`\uFEFF${FULL}`));
  });

  test("raw form feed outside a comment is rejected", () => {
    assert.throws(() => parsed(FULL.replace("{", "{\f")));
  });

  test("raw form feed inside a comment is accepted", () => {
    const body = FULL.replace("// a comment", "// a\f comment");
    assert.equal(resolveProjectConfig(parsed(body)).name, "demo-app");
  });

  test("raw control characters inside strings are rejected", () => {
    const body = FULL.replace(
      "https://control.zeroship.ai",
      "https://control.\nzeroship.ai",
    );
    assert.throws(() => parsed(body), /UnexpectedEndOfString/);
  });

  test("non-JSON Unicode whitespace is rejected", () => {
    assert.throws(() => parsed(FULL.replace("{", "{\u00a0")));
  });

  test("loose JSON extensions are rejected", () => {
    const cases = [
      FULL.replace('"name":', "name:"),
      FULL.replace('"name": "demo-app",\n  "control"', '"name": "demo-app"\n  "control"'),
      FULL.replace('"demo-app"', "'demo-app'"),
      FULL.replace('"demo-app"', "0x10"),
      FULL.replace('"demo-app"', "+1"),
    ];
    for (const body of cases) assert.throws(() => parsed(body));
  });

  test("__proto__ remains visible to unknown-key validation", () => {
    const body = FULL.replace(
      "{\n",
      '{\n  "__proto__": { "control": "https://attacker.invalid" },\n',
    );
    assert.throws(() => parsed(body), /__proto__/);
  });

  test("the committed fixture parses through the real file reader", () => {
    const fixture = join(
      import.meta.dirname,
      "..",
      "..",
      "..",
      "tests",
      "fixtures",
      "project-config",
      "zeroship.jsonc",
    );
    const r = resolveProjectConfig(loadProjectConfig(fixture));
    assert.equal(r.name, "config-fixture");
    assert.deepEqual(Object.keys(r.databases ?? {}), ["main", "analytics"]);
    assert.equal(r.databases!.main.out, "generated/zeroship/main");
    assert.deepEqual(r.apps!.storefront.databases, ["main", "analytics"]);
  });
});
