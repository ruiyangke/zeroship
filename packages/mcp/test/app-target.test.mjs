import assert from "node:assert/strict";
import test from "node:test";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import { createZeroshipMcpServer } from "../dist/index.js";

const APP_ID = "app_0000000002e4nenowz3qmamtd";
const RAW_UUID = "0197f8a1-2b3c-7d4e-8f90-1a2b3c4d5e6f";

function appRecord(name) {
  return {
    id: APP_ID,
    name,
    plan_id: "free",
    deploy_hash: null,
    archived_at: null,
    created_at: "2026-09-12T00:00:00Z",
    updated_at: "2026-09-12T00:00:00Z",
  };
}

async function withMcp(fetchImpl, run) {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = fetchImpl;
  const server = createZeroshipMcpServer({
    baseUrl: "http://control.test",
    token: "test-token",
  });
  const client = new Client({ name: "mcp-target-test", version: "1" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();

  try {
    await Promise.all([
      server.connect(serverTransport),
      client.connect(clientTransport),
    ]);
    await run(client);
  } finally {
    await client.close();
    await server.close();
    globalThis.fetch = originalFetch;
  }
}

test("deploy_app keeps typed ids and names as explicit target variants", async () => {
  const calls = [];
  await withMcp(async (input, init = {}) => {
    const url = String(input);
    const method = init.method ?? "GET";
    const body = typeof init.body === "string" ? JSON.parse(init.body) : null;
    calls.push({ method, url, body });

    if (method === "POST" && url.endsWith(`/${APP_ID}/deploy`)) {
      return Response.json({ deploy_hash: "sha256:existing" });
    }
    return new Response("unexpected request", { status: 599 });
  }, async (client) => {
    const result = await client.callTool({
      name: "deploy_app",
      arguments: {
        target: { kind: "id", appId: APP_ID },
        zshipPath: "/dev/null",
      },
    });
    assert.equal(result.isError, undefined);
  });
  assert.deepEqual(calls, [
    {
      method: "POST",
      url: `http://control.test/api/apps/${APP_ID}/deploy`,
      body: null,
    },
  ]);

  calls.length = 0;
  await withMcp(async (input, init = {}) => {
    const url = String(input);
    const method = init.method ?? "GET";
    const body = typeof init.body === "string" ? JSON.parse(init.body) : null;
    calls.push({ method, url, body });

    if (method === "GET" && url.endsWith("/api/apps")) {
      return Response.json([]);
    }
    if (method === "POST" && url.endsWith("/api/apps")) {
      return Response.json(appRecord(body.name), { status: 201 });
    }
    if (method === "POST" && url.endsWith(`/${APP_ID}/deploy`)) {
      return Response.json({ deploy_hash: "sha256:created" });
    }
    return new Response("unexpected request", { status: 599 });
  }, async (client) => {
    const result = await client.callTool({
      name: "deploy_app",
      arguments: {
        target: { kind: "name", appName: "notes" },
        zshipPath: "/dev/null",
      },
    });
    assert.equal(result.isError, undefined);
  });
  assert.deepEqual(calls, [
    { method: "GET", url: "http://control.test/api/apps", body: null },
    {
      method: "POST",
      url: "http://control.test/api/apps",
      body: { name: "notes", plan_id: "free" },
    },
    {
      method: "POST",
      url: `http://control.test/api/apps/${APP_ID}/deploy`,
      body: null,
    },
  ]);
});

test("deploy_app refuses raw UUIDs and the removed ambiguous app input", async () => {
  const calls = [];
  await withMcp(async (input, init) => {
    calls.push({ input: String(input), init });
    return new Response("must not be called", { status: 599 });
  }, async (client) => {
    const invalidId = await client.callTool({
      name: "deploy_app",
      arguments: {
        target: { kind: "id", appId: RAW_UUID },
        zshipPath: "/dev/null",
      },
    });
    assert.equal(invalidId.isError, true);
    assert.match(invalidId.content[0].text, /appId must be a canonical AppId/);

    const removedInput = await client.callTool({
      name: "deploy_app",
      arguments: { app: RAW_UUID, zshipPath: "/dev/null" },
    });
    assert.equal(removedInput.isError, true);
    assert.match(removedInput.content[0].text, /Invalid arguments/);
  });
  assert.deepEqual(calls, []);
});
