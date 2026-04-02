/**
 * Full workflow E2E test — API only (no browser needed).
 * Tests the complete user journey: create → deploy → call → logs → update → delete.
 */
import { test, expect } from "@playwright/test";

const API = "http://localhost:3335";
const KEY = "e2e-test-key";
const h = { Authorization: `Bearer ${KEY}`, "Content-Type": "application/json" };

test.describe("Full User Workflow", () => {
  const appId = `workflow-${Date.now()}`;
  let apiKey: string;

  test("1. Create app from template", async ({ request }) => {
    // Get templates
    const tplRes = await request.get(`${API}/api/templates`);
    expect(tplRes.status()).toBe(200);
    const templates = await tplRes.json();
    expect(templates.length).toBeGreaterThan(0);
    const todoTemplate = templates.find((t: any) => t.id === "todo-api");
    expect(todoTemplate).toBeTruthy();

    // Create app
    const createRes = await request.post(`${API}/api/apps`, {
      headers: h,
      data: { id: appId, plan_id: "free" },
    });
    expect([200, 201]).toContain(createRes.status());
    const app = await createRes.json();
    apiKey = app.api_key;
    expect(apiKey).toBeTruthy();

    // Deploy template code
    const deployRes = await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { Authorization: `Bearer ${KEY}`, "Content-Type": "application/javascript" },
      data: todoTemplate.code,
    });
    expect(deployRes.status()).toBe(200);
    const deploy = await deployRes.json();
    expect(deploy.version).toBe(1);
  });

  test("2. Call app RPC methods", async ({ request }) => {
    // Add a todo
    const addRes = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "add", params: ["Buy groceries"], id: 1 },
    });
    expect(addRes.status()).toBe(200);
    const addBody = await addRes.json();
    expect(addBody.result).toBeTruthy();

    // List todos
    const listRes = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "list", params: [], id: 2 },
    });
    expect(listRes.status()).toBe(200);
    const listBody = await listRes.json();
    expect(Array.isArray(listBody.result)).toBe(true);
    expect(listBody.result.length).toBe(1);
  });

  test("3. Check usage metering", async ({ request }) => {
    const usageRes = await request.get(`${API}/_apps/${appId}/usage`, { headers: h });
    expect(usageRes.status()).toBe(200);
    const usage = await usageRes.json();
    expect(usage.requests).toBeGreaterThan(0);
  });

  test("4. Check console logs", async ({ request }) => {
    const logsRes = await request.get(`${API}/api/apps/${appId}/logs`, { headers: h });
    expect(logsRes.status()).toBe(200);
    const logs = await logsRes.json();
    expect(Array.isArray(logs)).toBe(true);
  });

  test("5. View deployed code", async ({ request }) => {
    const appRes = await request.get(`${API}/api/apps/${appId}`, { headers: h });
    expect(appRes.status()).toBe(200);
    const app = await appRes.json();
    expect(app.server_js).toContain("__rpc");
    expect(app.version).toBe(1);
  });

  test("6. Redeploy with updated code", async ({ request }) => {
    const newCode = 'var __rpc = { ping: function() { return "pong v2"; } };';
    const deployRes = await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { Authorization: `Bearer ${KEY}`, "Content-Type": "application/javascript" },
      data: newCode,
    });
    expect(deployRes.status()).toBe(200);
    const deploy = await deployRes.json();
    expect(deploy.version).toBe(2);

    // Verify new code runs
    const rpcRes = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "ping", params: [], id: 1 },
    });
    const body = await rpcRes.json();
    expect(body.result).toBe("pong v2");
  });

  test("7. Change plan", async ({ request }) => {
    const planRes = await request.put(`${API}/api/apps/${appId}/plan`, {
      headers: h,
      data: { plan_id: "pro" },
    });
    expect(planRes.status()).toBe(200);

    const appRes = await request.get(`${API}/api/apps/${appId}`, { headers: h });
    const app = await appRes.json();
    expect(app.plan_id).toBe("pro");
  });

  test("8. Deploy with per-app API key", async ({ request }) => {
    const deployRes = await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { Authorization: `Bearer ${apiKey}`, "Content-Type": "application/javascript" },
      data: 'var __rpc = { ping: function() { return "pong v3"; } };',
    });
    expect(deployRes.status()).toBe(200);
  });

  test("9. Deploy with wrong key fails", async ({ request }) => {
    const deployRes = await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { Authorization: "Bearer wrong-key", "Content-Type": "application/javascript" },
      data: 'var __rpc = {};',
    });
    expect(deployRes.status()).toBe(401);
  });

  test("10. Pool stats show app", async ({ request }) => {
    const statsRes = await request.get(`${API}/_stats`, { headers: h });
    expect(statsRes.status()).toBe(200);
    const stats = await statsRes.json();
    expect(stats.active_isolates).toBeGreaterThan(0);
    const appStat = stats.apps.find((a: any) => a.app_id === appId);
    expect(appStat).toBeTruthy();
    expect(appStat.request_count).toBeGreaterThan(0);
  });

  test("11. Delete app", async ({ request }) => {
    const delRes = await request.delete(`${API}/api/apps/${appId}`, { headers: h });
    expect(delRes.status()).toBe(200);

    // Verify gone
    const getRes = await request.get(`${API}/api/apps/${appId}`, { headers: h });
    expect(getRes.status()).toBe(404);
  });
});

test.describe("Multi-tenant Isolation", () => {
  const app1 = `iso-a-${Date.now()}`;
  const app2 = `iso-b-${Date.now()}`;

  test.beforeAll(async ({ request }) => {
    await request.post(`${API}/api/apps`, { headers: h, data: { id: app1, plan_id: "free" } });
    await request.post(`${API}/api/apps`, { headers: h, data: { id: app2, plan_id: "free" } });
    await request.post(`${API}/api/apps/${app1}/deploy`, {
      headers: { ...h, "Content-Type": "application/javascript" },
      data: 'var __rpc = { get: function() { return kv.get("secret"); }, set: function(v) { kv.set("secret", v); return "ok"; } };',
    });
    await request.post(`${API}/api/apps/${app2}/deploy`, {
      headers: { ...h, "Content-Type": "application/javascript" },
      data: 'var __rpc = { get: function() { return kv.get("secret"); } };',
    });
  });

  test("KV data is isolated between apps", async ({ request }) => {
    // Set secret in app1
    await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": app1 },
      data: { jsonrpc: "2.0", method: "set", params: ["app1-secret"], id: 1 },
    });

    // App1 can read it
    const res1 = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": app1 },
      data: { jsonrpc: "2.0", method: "get", params: [], id: 2 },
    });
    expect((await res1.json()).result).toBe("app1-secret");

    // App2 cannot see it (isolated KV)
    const res2 = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": app2 },
      data: { jsonrpc: "2.0", method: "get", params: [], id: 3 },
    });
    expect((await res2.json()).result).toBeNull();
  });
});

test.describe("Error Handling", () => {
  test("Syntax error in deployed code doesn't crash", async ({ request }) => {
    const appId = `err-${Date.now()}`;
    await request.post(`${API}/api/apps`, { headers: h, data: { id: appId, plan_id: "free" } });

    // Deploy invalid JS
    await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { ...h, "Content-Type": "application/javascript" },
      data: "this is not valid javascript {{{}}}",
    });

    // Call should return an error, not crash
    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "anything", params: [], id: 1 },
    });
    expect(res.status()).toBe(200); // JSON-RPC errors are 200 with error body
    const body = await res.json();
    expect(body.error).toBeTruthy();
  });

  test("Unknown app returns error", async ({ request }) => {
    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": "nonexistent-app" },
      data: { jsonrpc: "2.0", method: "ping", params: [], id: 1 },
    });
    expect(res.status()).toBe(404);
  });
});
