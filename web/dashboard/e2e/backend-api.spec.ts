/**
 * Backend API E2E tests — tests the Rust server directly via HTTP.
 * No browser needed, just HTTP assertions.
 */
import { test, expect } from "@playwright/test";

const API = "http://localhost:3335";
const KEY = "e2e-test-key";

const headers = {
  Authorization: `Bearer ${KEY}`,
  "Content-Type": "application/json",
};

test.describe("Health & Stats", () => {
  test("GET /_health returns ok", async ({ request }) => {
    const res = await request.get(`${API}/_health`);
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.status).toBe("ok");
  });

  test("GET /_stats requires auth", async ({ request }) => {
    const res = await request.get(`${API}/_stats`);
    expect(res.status()).toBe(401);
  });

  test("GET /_stats with auth succeeds", async ({ request }) => {
    const res = await request.get(`${API}/_stats`, { headers });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body).toHaveProperty("active_isolates");
    expect(body).toHaveProperty("max_isolates");
  });
});

test.describe("App CRUD", () => {
  const appId = `e2e-app-${Date.now()}`;

  test("POST /api/apps creates app", async ({ request }) => {
    const res = await request.post(`${API}/api/apps`, {
      headers,
      data: { id: appId, plan_id: "free" },
    });
    expect([200, 201]).toContain(res.status());
    const body = await res.json();
    expect(body.id).toBe(appId);
    expect(body.api_key).toBeTruthy();
    expect(body.version).toBe(0);
  });

  test("POST /api/apps duplicate returns error", async ({ request }) => {
    // Create first
    await request.post(`${API}/api/apps`, {
      headers,
      data: { id: "dup-test", plan_id: "free" },
    });
    // Try duplicate
    const res = await request.post(`${API}/api/apps`, {
      headers,
      data: { id: "dup-test", plan_id: "free" },
    });
    expect(res.status()).toBe(409);
  });

  test("GET /api/apps lists apps", async ({ request }) => {
    const res = await request.get(`${API}/api/apps`, { headers });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(Array.isArray(body)).toBe(true);
    expect(body.length).toBeGreaterThan(0);
  });

  test("GET /api/apps requires auth", async ({ request }) => {
    const res = await request.get(`${API}/api/apps`);
    expect(res.status()).toBe(401);
  });

  test("GET /api/apps/:id returns app with code", async ({ request }) => {
    const res = await request.get(`${API}/api/apps/default`, { headers });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.id).toBe("default");
    expect(body).toHaveProperty("server_js");
  });

  test("DELETE /api/apps/:id deletes app", async ({ request }) => {
    await request.post(`${API}/api/apps`, {
      headers,
      data: { id: "to-delete", plan_id: "free" },
    });
    const res = await request.delete(`${API}/api/apps/to-delete`, { headers });
    expect(res.status()).toBe(200);
  });
});

test.describe("Deploy & RPC", () => {
  const appId = `deploy-test-${Date.now()}`;

  test.beforeAll(async ({ request }) => {
    await request.post(`${API}/api/apps`, {
      headers,
      data: { id: appId, plan_id: "free" },
    });
  });

  test("POST /api/apps/:id/deploy deploys code", async ({ request }) => {
    const res = await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { ...headers, "Content-Type": "application/javascript" },
      data: 'var __rpc = { add: function(a, b) { return a + b; }, greet: function(name) { console.log("Hello " + name); return "Hi " + name; } };',
    });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.version).toBe(1);
  });

  test("POST /rpc calls deployed app", async ({ request }) => {
    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "add", params: [3, 4], id: 1 },
    });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.result).toBe(7);
  });

  test("POST /rpc with unknown method returns error", async ({ request }) => {
    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "nonexistent", params: [], id: 1 },
    });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(body.error).toBeTruthy();
  });

  test("GET /api/apps/:id/logs returns logs after RPC", async ({ request }) => {
    // Call greet which does console.log
    await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "greet", params: ["World"], id: 1 },
    });

    const res = await request.get(`${API}/api/apps/${appId}/logs`, { headers });
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(Array.isArray(body)).toBe(true);
    expect(body.some((log: string) => log.includes("Hello World"))).toBe(true);
  });
});

test.describe("KV Store", () => {
  const appId = `kv-test-${Date.now()}`;

  test.beforeAll(async ({ request }) => {
    await request.post(`${API}/api/apps`, {
      headers,
      data: { id: appId, plan_id: "free" },
    });
    await request.post(`${API}/api/apps/${appId}/deploy`, {
      headers: { ...headers, "Content-Type": "application/javascript" },
      data: 'var __rpc = { set: function(k, v) { kv.set(k, v); return "ok"; }, get: function(k) { return kv.get(k); }, list: function() { return kv.list(); } };',
    });
  });

  test("KV set and get", async ({ request }) => {
    await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "set", params: ["name", "Alice"], id: 1 },
    });

    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "get", params: ["name"], id: 2 },
    });
    const body = await res.json();
    expect(body.result).toBe("Alice");
  });

  test("KV list returns keys", async ({ request }) => {
    const res = await request.post(`${API}/rpc`, {
      headers: { "Content-Type": "application/json", "X-App-Id": appId },
      data: { jsonrpc: "2.0", method: "list", params: [], id: 3 },
    });
    const body = await res.json();
    expect(Array.isArray(body.result)).toBe(true);
    expect(body.result).toContain("name");
  });
});

test.describe("Templates", () => {
  test("GET /api/templates returns templates", async ({ request }) => {
    const res = await request.get(`${API}/api/templates`);
    expect(res.status()).toBe(200);
    const body = await res.json();
    expect(Array.isArray(body)).toBe(true);
    expect(body.length).toBeGreaterThanOrEqual(1);
    expect(body[0]).toHaveProperty("id");
    expect(body[0]).toHaveProperty("code");
  });
});
