import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: ServicesSettings = {
  exampleDir,
  label: "Starter",
  workDir: "zeroship-starter-",
  cleanupMessage: "Failed to clean up the starter fixture",
  cancelMessage: "starter fixture cancelled",
  database: { name: "starter_fixture", password: "starter-fixture-password" },
  issuer: { kid: "starter-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read" },
  owner: { seed: "starter-fixture-owner", emailPrefix: "starter", name: "starter fixture owner" },
  signer: typedIdFromStableSeed("wjs", "starter-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "starter",
  buildNoun: "starter",
  // The browser holds no session, so the gateway admits these only as
  // anonymous resources.
  manifest: (manifest) => {
    for (const id of ["rpc:getMessages", "rpc:addMessage", "rpc:boom"]) {
      assert.equal(manifest.resources?.[id]?.auth, "anonymous", `${id} must admit the anonymous browser`);
    }
  },
  typedAppAssert: true,
  blobs: async (_backing, work) => {
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    return blobs;
  },
  devEnvVar: "STARTER_API_PORT",
  targets: ({ dev, ui, gateway, log }) => [
    { name: "dev", apiUrl: dev.url, uiUrl: ui.url, log: log("dev") },
    { name: "deployed", apiUrl: `${gateway.url}/apps/starter`, uiUrl: `http://starter.localhost:${gateway.number}`, log: log("worker") },
  ],
  readiness: () => ({ path: "/__zeroship/v1/getMessages", body: {} }),
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`Starter fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
