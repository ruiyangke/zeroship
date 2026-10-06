import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { WorkflowPlatform, prepare, type WorkflowSettings } from "@zeroship/example-testkit";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: WorkflowSettings = {
  exampleDir,
  label: "Workflow",
  workDir: "zeroship-workflows-order-",
  cleanupMessage: "Failed to clean up workflow fixture",
  cancelMessage: "Storage fixture cancelled",
  database: { name: "workflow_fixture", password: "workflow-fixture-password" },
  issuer: { kid: "workflow-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read secrets:read" },
  owner: { seed: "workflows-order-fixture-owner", emailPrefix: "probe", name: "Workflow fixture owner" },
  signer: typedIdFromStableSeed("wjs", "workflows-order-fixture-signer"),
  services: ["control", "gateway", "workflow"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server", "zeroship-workflow-server"],
  appName: "orders",
  manifest: (manifest) => {
    const resources = Object.entries(manifest.resources as Record<string, { auth?: unknown }>);
    assert(resources.length > 0, "Fixture must expose HTTP resources");
    assert.deepEqual([...manifest.workflows].sort(), ["OrderWorkflow", "RiskReviewWorkflow"].sort(), "Build must preserve workflow class names");
    assert(resources.every(([, resource]) => resource.auth === "anonymous"), "Workflow example HTTP resources must declare anonymous access");
  },
  blobs: async (_backing, work) => join(work, "blobs"),
  dev: { kind: "serve" },
  targets: ({ dev, gateway }) => [
    { name: "local", apiUrl: dev.url, uiUrl: dev.url },
    { name: "deployed", apiUrl: gateway.url + "/apps/orders", uiUrl: "http://orders.localhost:" + gateway.number },
  ],
  readiness: () => ({ path: "/" }),
  gate: {
    startRequest: () => ({
      path: "/orders",
      init: { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ orderId: randomUUID(), sku: "readiness", quantity: 1 }) },
    }),
    accepted: (body) => typeof body?.runId === "string",
    run: (body) => body.runId,
    statusRequest: (_target, run) => ({ path: "/orders/" + run }),
    state: (body) => body?.state,
    expected: "sleeping",
  },
};

export class Platform extends WorkflowPlatform {
  static async create(): Promise<Platform> {
    const { work, logs } = await prepare(settings);
    console.info(`Workflow fixture logs: ${logs}`);
    return new Platform(work, logs);
  }

  constructor(work: string, logs: string) {
    super(settings, work, logs);
  }
}
