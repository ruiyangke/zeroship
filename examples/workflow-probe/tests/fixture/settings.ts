import assert from "node:assert/strict";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { PlatformBase, WorkflowPlatform, prepare, type WorkflowSettings } from "@zeroship/example-fixtures";
import { typedIdFromStableSeed } from "@zeroship/server/typed-id";

const exampleDir = fileURLToPath(new URL("../../", import.meta.url));

const settings: WorkflowSettings = {
  exampleDir,
  label: "Workflow",
  workDir: "zeroship-workflow-probe-",
  cleanupMessage: "Failed to clean up workflow fixture",
  cancelMessage: "Storage fixture cancelled",
  database: { name: "workflow_fixture", password: "workflow-fixture-password" },
  issuer: { kid: "workflow-acceptance", scope: "organization:create apps:read apps:write apps:deploy deployments:read secrets:read" },
  owner: { seed: "workflow-probe-fixture-owner", emailPrefix: "probe", name: "Workflow fixture owner" },
  signer: typedIdFromStableSeed("wjs", "workflow-probe-fixture-signer"),
  services: ["control", "gateway", "workflow"],
  cargoPackages: ["zeroship-cli", "zeroship-worker", "zeroship-control", "zeroship-gateway", "zeroship-data-cdc-server", "zeroship-migrate-server", "zeroship-workflow-server"],
  appName: "probe",
  manifest: (manifest) => {
    const resources = Object.entries(manifest.resources as Record<string, { auth?: unknown }>)
      .filter(([id]) => id.startsWith("rpc:"));
    assert(resources.length > 0, "Fixture must expose RPC resources");
    assert.deepEqual([...manifest.workflows].sort(), ["BasicCase", "SleepCase", "SignalCase", "ChildCase", "CompensateCase", "DoubleChild"].sort(), "Build must preserve workflow class names");
    assert(resources.every(([, resource]) => resource.auth === "anonymous"), "Workflow example RPC resources must declare anonymous access");
  },
  backing: PlatformBase.redisBacking,
  workerExtraArgs: (backing) => ["--kv-config-file", backing.kvConfig!],
  blobs: async (_backing, work) => join(work, "blobs"),
  dev: { kind: "vite", envVar: "WORKFLOW_PROBE_API_PORT" },
  targets: ({ dev, ui, gateway }) => [
    { name: "local", apiUrl: dev.url, uiUrl: ui.url },
    { name: "deployed", apiUrl: `${gateway.url}/apps/probe`, uiUrl: `http://probe.localhost:${gateway.number}` },
  ],
  readiness: () => ({
    path: "/__zeroship/v1/wf.ping",
    init: { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: {} }) },
  }),
  gate: {
    startRequest: () => ({
      path: "/__zeroship/v1/wf.start",
      init: { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: { case: "basic" } }) },
    }),
    accepted: (body) => typeof body?.json?.runId === "string",
    run: (body) => body.json,
    statusRequest: (_target, run) => ({
      path: "/__zeroship/v1/wf.status",
      init: { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: run }) },
    }),
    state: (body) => body?.json?.state,
    expected: "completed",
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
