import { randomUUID } from "node:crypto";
import { expect, test } from "vitest";
import { targets, type Target } from "./targets";

async function request(target: Target, path: string, body?: unknown) {
  const response = await fetch(target.apiUrl + path, {
    method: body === undefined ? "GET" : "POST", headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15_000),
  });
  const text = await response.text();
  expect(response.ok, `${target.name} ${path}: ${response.status}: ${text}`).toBe(true);
  return JSON.parse(text);
}

test("the raw order app joins duplicate starts and handles the approval signal", async () => {
  for (const target of targets()) {
    const orderId = randomUUID();
    const input = { orderId, sku: "hat", quantity: 2 };
    const started = await request(target, "/orders", input);
    expect(started.runId).toMatch(/^run_/);
    expect((await request(target, "/orders", input)).runId).toBe(started.runId);
    const path = "/orders/" + started.runId;
    // The parent first waits for its child, then sleeps before awaiting payment.
    await expect.poll(async () => (await request(target, path)).state, { timeout: 60_000 }).toBe("sleeping");
    await expect.poll(async () => (await request(target, path)).state, { timeout: 60_000 }).toBe("waiting");
    await request(target, path + "/approve", { approved: true, approvalCode: "approved-" + orderId });
    await expect.poll(async () => (await request(target, path)).state, { timeout: 30_000 }).toBe("completed");
    const result = await request(target, path);
    expect(result.output).toMatchObject({ orderId, status: "ready_to_ship", shipmentId: "shp_" + orderId });
    expect(result.output.riskScore).toBeGreaterThanOrEqual(0);
    expect(result.output.riskScore).toBeLessThan(100);
  }
});
