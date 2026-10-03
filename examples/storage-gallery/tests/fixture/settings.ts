import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: ServicesSettings = {
  exampleDir,
  label: "Storage",
  workDir: "zeroship-storage-gallery-",
  cleanupMessage: "Failed to clean up storage fixture",
  cancelMessage: "Storage fixture cancelled",
  database: { name: "storage_fixture", password: "storage-fixture-password" },
  issuer: { kid: "storage-gallery-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read secrets:read" },
  owner: { seed: "storage-gallery-fixture-owner", emailPrefix: "gallery", name: "Storage fixture owner" },
  signer: typedIdFromStableSeed("wjs", "storage-gallery-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "gallery",
  buildNoun: "storage",
  manifest: (manifest) => {
    const resources = Object.entries(manifest.resources as Record<string, { auth?: unknown }>)
      .filter(([id]) => id.startsWith("rpc:"));
    assert(resources.length > 0, "Fixture must expose RPC resources");
    assert(resources.every(([, resource]) => resource.auth === "anonymous"), "Storage example RPC resources must declare anonymous access");
  },
  typedAppAssert: true,
  plan: "streaming",
  backing: ServicesPlatform.s3Backing,
  sharedEnv: () => ({ AWS_ACCESS_KEY_ID: "zeroship-fixture", AWS_SECRET_ACCESS_KEY: "zeroship-fixture-secret" }),
  workerExtraArgs: (backing) => ["--storage-url", backing.storeUrl!("objects")],
  blobs: async (backing) => backing.storeUrl!("deploy"),
  devEnvVar: "STORAGE_GALLERY_API_PORT",
  targets: ({ dev, ui, gateway }) => [
    { name: "local", apiUrl: dev.url, uiUrl: ui.url },
    { name: "s3", apiUrl: `${gateway.url}/apps/gallery`, uiUrl: `http://gallery.localhost:${gateway.number}` },
  ],
  readiness: () => ({ path: "/__zeroship/v1/gallery.list", body: {} }),
  postDeploy: async ({ id, gateway, worker, backing, workerProcess, keys }) => {
    const storedBlobs = await backing.s3!.exec(["/bin/sh", "-c", "find /data/storage-fixture/deploy/blobs -type f"]);
    assert.equal(storedBlobs.exitCode, 0, storedBlobs.output);
    assert(storedBlobs.output.trim(), "Deploy must write blobs into S3");
    assert(workerProcess.child.pid, "Worker process must have a PID");
    return {
      s3: { endpoint: backing.endpoint!, bucket: "storage-fixture", prefix: "objects", appId: id, workerPid: workerProcess.child.pid },
      worker: { url: worker.url, appId: id, gatewayKey: await readFile(keys.gateway, "utf8") },
    };
  },
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`Storage fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
