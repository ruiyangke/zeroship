import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import { test } from "node:test";
import {
  discoverZeroMigrateConfig,
  loadZeroMigrateConfig,
  resolveCliConfig,
} from "../../src/config.js";

function temporaryProject(): { root: string; child: string; config: string } {
  const root = mkdtempSync(join(tmpdir(), "zero-migrate-config-"));
  const child = join(root, "packages", "service");
  mkdirSync(child, { recursive: true });
  return { root, child, config: join(root, "zero-migrate.toml") };
}

test("config discovery walks upward and defaults to the dev environment", () => {
  const project = temporaryProject();
  try {
    writeFileSync(
      project.config,
      `[env.production]
url = "postgres://production.example/app"

[env.dev]
dir = "./db/migrations"
owner_app = "app_dev"
schema = "app_data"
registry = "./db/registry.json"
policy = ["./policy/root.toml", "./policy/team.toml"]
`,
    );

    assert.equal(discoverZeroMigrateConfig(project.child), project.config);
    const loaded = loadZeroMigrateConfig({ cwd: project.child, processEnv: {} });
    assert.equal(loaded?.environment, "dev");
    assert.deepEqual(loaded?.values, {
      url: undefined,
      dir: resolve(project.root, "db/migrations"),
      ownerApp: "app_dev",
      schema: "app_data",
      registry: resolve(project.root, "db/registry.json"),
      policy: [
        resolve(project.root, "policy/root.toml"),
        resolve(project.root, "policy/team.toml"),
      ],
    });
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});

test("a sole non-dev environment is selected and --env overrides it", () => {
  const project = temporaryProject();
  try {
    writeFileSync(project.config, `[env.staging]\nowner_app = "app_staging"\n`);
    assert.equal(
      loadZeroMigrateConfig({ cwd: project.child, processEnv: {} })?.environment,
      "staging",
    );
    assert.throws(
      () =>
        loadZeroMigrateConfig({
          cwd: project.child,
          environment: "missing",
          processEnv: {},
        }),
      /environment "missing" is not defined/,
    );

    writeFileSync(
      project.config,
      `[env.staging]\nowner_app = "app_staging"\n[env.production]\nowner_app = "app_prod"\n`,
    );
    assert.throws(
      () => loadZeroMigrateConfig({ cwd: project.child, processEnv: {} }),
      /multiple environments.*--env/,
    );
    assert.equal(
      loadZeroMigrateConfig({
        cwd: project.child,
        environment: "production",
        processEnv: {},
      })?.values.ownerApp,
      "app_prod",
    );
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});

test("config env: references resolve set variables and reject unset variables", () => {
  const project = temporaryProject();
  try {
    writeFileSync(
      project.config,
      `[env.dev]
url = "env:TEST_DATABASE_URL"
policy = "env:TEST_POLICY_PATH"
`,
    );
    const loaded = loadZeroMigrateConfig({
      cwd: project.child,
      processEnv: {
        TEST_DATABASE_URL: "postgres://db.example/app",
        TEST_POLICY_PATH: "./policy.toml",
      },
    });
    assert.equal(loaded?.values.url, "postgres://db.example/app");
    assert.deepEqual(loaded?.values.policy, [resolve(project.root, "policy.toml")]);

    assert.throws(
      () =>
        loadZeroMigrateConfig({
          cwd: project.child,
          processEnv: { TEST_POLICY_PATH: "./policy.toml" },
        }),
      /references unset environment variable TEST_DATABASE_URL/,
    );
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});

test("setting precedence is flags, environment, config, then defaults", () => {
  const project = temporaryProject();
  try {
    writeFileSync(
      project.config,
      `[env.dev]
url = "postgres://config.example/app"
dir = "./config-migrations"
owner_app = "app_config"
schema = "config_schema"
registry = "./config-registry.json"
policy = ["./config-root.toml", "./config-leaf.toml"]
`,
    );
    const resolved = resolveCliConfig({
      cwd: project.child,
      explicit: {
        databaseUrl: "postgres://flag.example/app",
        dir: "./flag-migrations",
        policyPaths: ["./flag-policy.toml"],
      },
      processEnv: {
        ZERO_MIGRATE_URL: "postgres://env.example/app",
        ZERO_MIGRATE_DIR: "./env-migrations",
        ZERO_MIGRATE_OWNER_APP: "app_env",
        ZERO_MIGRATE_REGISTRY: "./env-registry.json",
        ZERO_MIGRATE_POLICY: "./env-policy.toml",
      },
      defaults: { projectSchema: "default_schema" },
    });

    assert.equal(resolved.databaseUrl, "postgres://flag.example/app");
    assert.equal(resolved.dir, "./flag-migrations");
    assert.equal(resolved.ownerApp, "app_env");
    assert.equal(resolved.projectSchema, "config_schema");
    assert.equal(resolved.registryPath, "./env-registry.json");
    assert.deepEqual(resolved.policyPaths, ["./flag-policy.toml"]);

    const configOverLegacyUrl = resolveCliConfig({
      cwd: project.child,
      processEnv: { DATABASE_URL: "postgres://legacy.example/app" },
    });
    assert.equal(configOverLegacyUrl.databaseUrl, "postgres://config.example/app");

    const zeroMigrateOverConfigUrl = resolveCliConfig({
      cwd: project.child,
      processEnv: {
        DATABASE_URL: "postgres://legacy.example/app",
        ZERO_MIGRATE_URL: "postgres://zero-migrate.example/app",
      },
    });
    assert.equal(
      zeroMigrateOverConfigUrl.databaseUrl,
      "postgres://zero-migrate.example/app",
    );
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});

test("ZERO_MIGRATE_POLICY carries multiple layers via the path delimiter", () => {
  // A single-valued env var used to collapse a multi-layer policy to one layer;
  // the env var now expresses an ordered layer list like PATH.
  const resolved = resolveCliConfig({
    processEnv: {
      ZERO_MIGRATE_POLICY: ["./root.toml", "./team.toml", "./svc.toml"].join(
        delimiter,
      ),
    },
  });
  assert.deepEqual(resolved.policyPaths, [
    "./root.toml",
    "./team.toml",
    "./svc.toml",
  ]);
  assert.deepEqual(resolved.warnings, []);
});

test("ZERO_MIGRATE_POLICY with blank/whitespace layers is treated as absent", () => {
  const resolved = resolveCliConfig({
    processEnv: { ZERO_MIGRATE_POLICY: `  ${delimiter} ${delimiter} ` },
  });
  // No layers -> no policy (not a single empty-string layer), and no warning.
  assert.deepEqual(resolved.policyPaths, []);
  assert.deepEqual(resolved.warnings, []);
});

test("ZERO_MIGRATE_POLICY overriding a config policy warns about dropped layers", () => {
  const project = temporaryProject();
  try {
    writeFileSync(
      project.config,
      `[env.dev]
policy = ["./config-root.toml", "./config-team.toml"]
`,
    );
    const resolved = resolveCliConfig({
      cwd: project.child,
      processEnv: { ZERO_MIGRATE_POLICY: "./env-only.toml" },
    });
    // Env wins (single layer), but the collapse is surfaced, not silent.
    assert.deepEqual(resolved.policyPaths, ["./env-only.toml"]);
    assert.equal(resolved.warnings.length, 1);
    assert.match(resolved.warnings[0], /ZERO_MIGRATE_POLICY \(1 layer\)/);
    assert.match(resolved.warnings[0], /2-layer policy/);
    assert.match(resolved.warnings[0], /widen effective policy/);
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});

test("no config preserves DATABASE_URL and built-in CLI defaults", () => {
  const project = temporaryProject();
  try {
    assert.deepEqual(
      resolveCliConfig({
        cwd: project.child,
        processEnv: { DATABASE_URL: "postgres://environment.example/app" },
      }),
      {
        databaseUrl: "postgres://environment.example/app",
        dir: "./migrations",
        ownerApp: "app_cli",
        projectSchema: "public",
        registryPath: undefined,
        policyPaths: [],
        configPath: undefined,
        environment: undefined,
        warnings: [],
      },
    );
  } finally {
    rmSync(project.root, { recursive: true, force: true });
  }
});
