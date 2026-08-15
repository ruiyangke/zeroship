/**
 * The TypeScript reader for `zeroship.jsonc`.
 *
 * WHAT THESE DO NOT CATCH, so their greenness is not overread:
 *
 * - They never compare this reader against the Rust one. Byte-equality of the
 *   two resolved dumps is checked by `tests/project_config_gate.sh` and needs
 *   both binaries; a divergent Rust default is invisible here.
 * - They do not run a real `vite build`, so nothing here proves the plugin
 *   actually threads the resolved config into the packer. That is the
 *   end-to-end harness's job (`tests/e2e_project_config.sh`).
 * - The escape-hatch tests prove the deny-list REFUSES; they do not prove the
 *   deny-list is complete, because completeness is a property of the schema's
 *   `x-cli-read` markers, not of this file. `deny_list_is_generated_not_typed`
 *   below is the closest thing, and it only checks the list is non-trivial and
 *   contains the fields the CLI is known to read.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

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
  type ResolvedProjectConfig,
} from "../src/project-config/index.js";

const FULL = `{
  // a comment, which is the whole reason the format is JSONC
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "demo-app",
  "app": "11111111-1111-4111-8111-111111111111",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": ["STRIPE_SECRET_KEY"],
  "environments": {
    "staging": {
      "app": "22222222-2222-4222-8222-222222222222",
      "control": "https://control.staging.zeroship.ai",
      "protected": true,
      "migrations": { "out": "generated/staging" }
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

  test("an environment missing app or control is refused as NON-INHERITABLE", () => {
    const body = FULL.replace('"app": "22222222-2222-4222-8222-222222222222",\n      ', "");
    assert.throws(() => parseProjectConfig("zeroship.jsonc", body), /NON-INHERITABLE/);
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
});

describe("resolution", () => {
  test("the root resolution drops $schema and environments", () => {
    const r = resolveProjectConfig(parsed());
    assert.equal(r.app, "11111111-1111-4111-8111-111111111111");
    assert.equal(r.migrations.out, "generated/zeroship");
    assert.ok(!("environments" in (r as unknown as Record<string, unknown>)));
    assert.ok(!("$schema" in (r as unknown as Record<string, unknown>)));
  });

  test("an environment replaces app/control and merges migrations member by member", () => {
    const r = resolveProjectConfig(parsed(), "staging");
    assert.equal(r.app, "22222222-2222-4222-8222-222222222222");
    assert.equal(r.control, "https://control.staging.zeroship.ai");
    assert.equal(r.migrations.out, "generated/staging");
    assert.equal(r.migrations.dir, "migrations", "unstated members inherit");
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
      assert.equal(config.migrations.dir, "migrations");
      assert.equal(config.migrations.out, "generated/zeroship");
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
    assert.equal(out.app, base.app);
    assert.equal(out.control, base.control);
  });

  test("changing a CLI-read field is refused, naming the field", () => {
    const base = resolveProjectConfig(parsed());
    for (const [label, partial] of [
      ["control", { control: "https://elsewhere.example" }],
      ["migrations.out", { migrations: { dir: "migrations", out: "somewhere/else" } }],
      ["build.output", { build: { ...base.build, output: "other.zship" } }],
      ["app", { app: "33333333-3333-4333-8333-333333333333" }],
      ["name", { name: "another-app" }],
      ["runtime_date", { runtime_date: "2027-01-01" }],
    ] as const) {
      assert.throws(
        () => applyProjectConfigOverride(base, partial as never),
        new RegExp(`may not change \\\`${label.replace(".", "\\.")}\\\``),
        `${label} must be refused`,
      );
    }
  });

  test("in-place changes to CLI-read fields are refused", () => {
    const mutations: Array<[string, (c: ResolvedProjectConfig) => void]> = [
      ["app", (c) => { c.app = "33333333-3333-4333-8333-333333333333"; }],
      ["migrations.out", (c) => { c.migrations.out = "somewhere/else"; }],
      ["build.output", (c) => { c.build.output = "other.zship"; }],
      ["control", (c) => { delete (c as unknown as Record<string, unknown>).control; }],
    ];

    for (const [field, mutate] of mutations) {
      const base = resolveProjectConfig(parsed());
      assert.throws(
        () => applyProjectConfigOverride(base, (c) => {
          mutate(c);
          return {};
        }),
        new RegExp(`may not change \\\`${field.replace(".", "\\.")}\\\``),
        `${field} must be refused after an in-place change`,
      );
    }
  });

  test("the deny-list is the generated one, not a list typed here", () => {
    // Generated from the schema's `x-cli-read` markers. Asserted rather than
    // assumed because a hand-maintained deny-list is exactly the drift the
    // restriction exists to prevent.
    for (const f of ["name", "app", "control", "runtime_date", "build.output", "migrations.dir", "migrations.out"]) {
      assert.ok(CLI_READ_FIELDS.includes(f), `${f} must be in CLI_READ_FIELDS`);
    }
  });
});

describe("the canonical dump", () => {
  test("keys are sorted and the output is compact", () => {
    const dump = canonicalJson(resolveProjectConfig(parsed()));
    assert.ok(dump.startsWith('{"app":'), dump);
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
      '"migrations":{"dir":"m","out":"g",},}';
    const r = resolveProjectConfig(parseProjectConfig("zeroship.jsonc", body));
    assert.equal(r.build.serverEntry, 'x"/*y*/,}-café-😀.ts');
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
    assert.equal(r.migrations.out, "generated/zeroship");
  });
});
