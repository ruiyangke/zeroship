import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: ServicesSettings = {
  exampleDir,
  label: "KV",
  workDir: "zeroship-kv-dashboard-",
  cleanupMessage: "Failed to clean up dashboard fixture",
  cancelMessage: "Dashboard fixture cancelled",
  database: { name: "kv_fixture", password: "kv-fixture-password" },
  issuer: { kid: "kv-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read secrets:read" },
  owner: { seed: "kv-dashboard-fixture-owner", emailPrefix: "kv", name: "KV fixture owner" },
  signer: typedIdFromStableSeed("wjs", "kv-dashboard-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "kvdash",
  buildNoun: "dashboard",
  manifest: (manifest) => {
    const resources = Object.entries(manifest.resources as Record<string, { auth?: unknown }>)
      .filter(([id]) => id.startsWith("rpc:"));
    assert(resources.length > 0, "Fixture must expose RPC resources");
    assert(resources.every(([, resource]) => resource.auth !== undefined), "RPC resource missing auth posture");
  },
  backing: ServicesPlatform.redisBacking,
  workerExtraArgs: (backing) => ["--kv-config-file", backing.kvConfig!],
  blobs: async (_backing, work) => {
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    return blobs;
  },
  devEnvVar: "KV_DASHBOARD_API_PORT",
  targets: ({ dev, ui, gateway }) => [
    { name: "redb", apiUrl: dev.url, uiUrl: ui.url },
    { name: "redis", apiUrl: `${gateway.url}/apps/kvdash`, uiUrl: `http://kvdash.localhost:${gateway.number}` },
  ],
  readiness: () => ({ path: "/__zeroship/v1/kv.snapshot", body: {} }),
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`KV fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
