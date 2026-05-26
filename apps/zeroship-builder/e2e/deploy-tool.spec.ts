import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

import { test, expect } from "@playwright/test";

import {
  DEV_SANDBOX_USER_ID,
  ZeroshipSandboxBackend,
  getOrCreateSandboxFor,
} from "../src/server/internal/sandbox-backend";
import { createDeployTool } from "../src/server/internal/tools";

const CONTROL_URL = process.env.CONTROL_URL ?? "http://localhost:9090";
const CONTROL_KEY = process.env.CONTROL_KEY ?? "dev-master-key";
const SANDBOX_URL = process.env.SANDBOX_URL ?? "http://localhost:9091";
const HAS_OPENAI_KEY = !!process.env.OPENAI_API_KEY;
const STEP_TIMEOUT = 300_000;

async function probe(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) });
    return res.ok || res.status === 401 || res.status === 404;
  } catch {
    return false;
  }
}

async function controlFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${CONTROL_KEY}`);
  if (init.body && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  return fetch(`${CONTROL_URL}${path}`, { ...init, headers });
}

async function createControlApp(name: string): Promise<{ id: string; name: string }> {
  const res = await controlFetch("/api/apps", {
    method: "POST",
    body: JSON.stringify({ name, plan_id: "free" }),
  });
  if (!res.ok) {
    throw new Error(`create app failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as { id: string; name: string };
}

async function getControlApp(appId: string): Promise<{ deploy_hash: string | null }> {
  const res = await controlFetch(`/api/apps/${encodeURIComponent(appId)}`);
  if (!res.ok) {
    throw new Error(`get app failed (${res.status}): ${await res.text()}`);
  }
  return (await res.json()) as { deploy_hash: string | null };
}

function parseToolJson(raw: unknown): any {
  const text = typeof raw === "string" ? raw : String(raw);
  return JSON.parse(text);
}

test.describe("Builder deploy tool hard gate", () => {
  test("blocks unsafe changes via real Reviewer and deploys clean .zship to real control plane", async ({}, testInfo) => {
    test.skip(!HAS_OPENAI_KEY, "OPENAI_API_KEY not set — real Reviewer call required");
    const [controlUp, sandboxUp] = await Promise.all([
      probe(`${CONTROL_URL}/health`),
      probe(`${SANDBOX_URL}/health`),
    ]);
    test.skip(!controlUp, `control plane unreachable at ${CONTROL_URL}`);
    test.skip(!sandboxUp, `sandbox controller unreachable at ${SANDBOX_URL}`);
    testInfo.setTimeout(STEP_TIMEOUT);

    const app = await createControlApp(`b3-deploy-tool-${Date.now()}`);
    const sandbox = await getOrCreateSandboxFor(`b3-deploy-tool-${app.id}`, {
      userId: DEV_SANDBOX_USER_ID,
      projectSourceId: app.id,
    });
    const backend = new ZeroshipSandboxBackend({
      id: sandbox.id,
      userId: sandbox.userId,
    });
    const deployTool = createDeployTool({
      backend,
      appId: app.id,
      apiKey: process.env.OPENAI_API_KEY,
    });

    await backend.write(
      "src/index.ts",
      [
        "const stripeSecret = 'sk_live_51N2_fake_hardcoded_secret_should_block';",
        "export function runUserCode(userInput: string) {",
        "  return eval(userInput);",
        "}",
        "export default { fetch: () => new Response(stripeSecret) };",
      ].join("\n"),
    );

    const blocked = parseToolJson(await deployTool.invoke({
      changes:
        "Attempting to deploy a server module with a hardcoded Stripe secret and eval(userInput).",
    }));

    expect(blocked.blocked).toBe(true);
    expect(blocked.reviewer_approved).toBe(false);
    expect(blocked.blockers.length).toBeGreaterThan(0);
    await expect.poll(async () => (await getControlApp(app.id)).deploy_hash).toBeNull();

    const fixture = await readFile(
      fileURLToPath(new URL("../dist/app.zship", import.meta.url)),
    );
    await backend.uploadFiles([
      ["fixture.zship", new Uint8Array(fixture)],
    ]);
    await backend.write(
      "package.json",
      JSON.stringify(
        {
          type: "module",
          scripts: {
            build: "mkdir -p dist && cp fixture.zship dist/app.zship",
          },
        },
        null,
        2,
      ),
    );
    await backend.write(
      "src/index.ts",
      [
        "export default {",
        "  fetch() {",
        "    return new Response('ok', { headers: { 'content-type': 'text/plain' } });",
        "  },",
        "};",
      ].join("\n"),
    );

    const deployed = parseToolJson(await deployTool.invoke({
      changes:
        "Deploying a clean zeroship app with a simple public fetch handler and no secrets.",
    }));

    expect(deployed.reviewer_approved).toBe(true);
    expect(deployed.deploy_hash).toMatch(/^[0-9a-f]{64}$/);
    expect(deployed.url).toBe(`/apps/${encodeURIComponent(app.name)}/`);
    await expect.poll(async () => (await getControlApp(app.id)).deploy_hash).toBe(
      deployed.deploy_hash,
    );
  });
});
