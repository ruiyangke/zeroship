import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ServicesPlatform, prepare, type ServicesSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: ServicesSettings = {
  exampleDir,
  label: "CSR",
  workDir: "zeroship-csr-todo-",
  cleanupMessage: "Failed to clean up the csr-todo fixture",
  cancelMessage: "csr-todo fixture cancelled",
  database: { name: "csr_fixture", password: "csr-fixture-password" },
  issuer: { kid: "csr-todo-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read" },
  owner: { seed: "csr-todo-fixture-owner", emailPrefix: "csr", name: "csr-todo fixture owner" },
  signer: typedIdFromStableSeed("wjs", "csr-todo-fixture-signer"),
  services: ["control", "gateway"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server"],
  appName: "csr-todo",
  buildNoun: "csr-todo",
  // The browser holds no session, so the gateway admits these two only as
  // anonymous resources.
  manifest: (manifest) => {
    for (const id of ["rpc:listTodos", "rpc:searchTodos"]) {
      assert.equal(manifest.resources?.[id]?.auth, "anonymous", `${id} must admit the anonymous browser`);
    }
  },
  typedAppAssert: true,
  blobs: async (_backing, work) => {
    const blobs = join(work, "blobs");
    await mkdir(blobs);
    return blobs;
  },
  devEnvVar: "CSR_TODO_API_PORT",
  targets: ({ dev, ui, gateway, log }) => [
    { name: "dev", apiUrl: dev.url, uiUrl: ui.url, log: log("dev") },
    { name: "deployed", apiUrl: `${gateway.url}/apps/csr-todo`, uiUrl: `http://csr-todo.localhost:${gateway.number}`, log: log("worker") },
  ],
  readiness: () => ({ path: "/__zeroship/v1/listTodos", body: {} }),
};

export class Platform extends ServicesPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`CSR fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
