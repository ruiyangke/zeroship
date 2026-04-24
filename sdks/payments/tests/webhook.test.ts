import { describe, it, expect } from "vitest";
import { verifyWebhook, signWebhookForTest } from "../src/webhook";
import { buildCheckoutSession } from "../src/checkout";

const SECRET = "whsec_test_EXAMPLE";
const NOW = 1_700_000_000;

describe("verifyWebhook", () => {
  it("accepts a freshly-signed body", async () => {
    const body = '{"type":"invoice.paid","data":{"object":{"amount_paid":1000}}}';
    const header = await signWebhookForTest(body, SECRET, NOW);
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: true, timestamp: NOW });
  });

  it("rejects a tampered body", async () => {
    const body = '{"amount_paid":1000}';
    const header = await signWebhookForTest(body, SECRET, NOW);
    const result = await verifyWebhook(body + " ", header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "signature mismatch" });
  });

  it("rejects a wrong secret", async () => {
    const body = '{"ok":true}';
    const header = await signWebhookForTest(body, SECRET, NOW);
    const result = await verifyWebhook(body, header, "wrong-secret", { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "signature mismatch" });
  });

  it("rejects a stale timestamp outside the default tolerance", async () => {
    const body = '{"x":1}';
    const header = await signWebhookForTest(body, SECRET, NOW - 400); // 400s old > 300s default
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "stale" });
  });

  it("allows stale if tolerance is raised", async () => {
    const body = '{"x":1}';
    const header = await signWebhookForTest(body, SECRET, NOW - 400);
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW, tolerance: 600 });
    expect(result).toEqual({ valid: true, timestamp: NOW - 400 });
  });

  it("rejects a header missing the v1 element", async () => {
    const body = '{"x":1}';
    const result = await verifyWebhook(body, `t=${NOW}`, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "missing t/v1" });
  });

  it("rejects a header with a non-numeric timestamp", async () => {
    const body = '{"x":1}';
    const result = await verifyWebhook(body, "t=oops,v1=abcd", SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "bad t" });
  });

  it("rejects an empty header", async () => {
    const result = await verifyWebhook('{"x":1}', "", SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "missing t/v1" });
  });

  it("accepts a header with extra v0 (legacy) parts", async () => {
    const body = '{"x":1}';
    const base = await signWebhookForTest(body, SECRET, NOW);
    const header = `${base},v0=deadbeef`;
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: true, timestamp: NOW });
  });

  it("tolerates whitespace in the header", async () => {
    const body = '{"x":1}';
    const base = await signWebhookForTest(body, SECRET, NOW);
    const header = base.split(",").map((p) => ` ${p} `).join(",");
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: true, timestamp: NOW });
  });
});

describe("buildCheckoutSession", () => {
  const base = {
    priceId: "price_123",
    creatorAccountId: "acct_abc",
    successUrl: "https://app.example/ok",
    cancelUrl: "https://app.example/cancel",
  };

  it("builds a POST request with the default 15% fee", () => {
    const req = buildCheckoutSession("sk_test_x", base);
    expect(req.method).toBe("POST");
    expect(req.url).toBe("https://api.stripe.com/v1/checkout/sessions");
    expect(req.headers.authorization).toBe("Bearer sk_test_x");
    expect(req.headers["stripe-account"]).toBe("acct_abc");
    expect(req.headers["content-type"]).toBe("application/x-www-form-urlencoded");
    expect(req.body).toContain("mode=subscription");
    expect(req.body).toContain("line_items%5B0%5D%5Bprice%5D=price_123");
    expect(req.body).toContain("subscription_data%5Bapplication_fee_percent%5D=15");
    expect(req.body).toContain("success_url=https%3A%2F%2Fapp.example%2Fok");
  });

  it("respects a custom applicationFeePercent", () => {
    const req = buildCheckoutSession("sk", { ...base, applicationFeePercent: 10 });
    expect(req.body).toContain("application_fee_percent%5D=10");
  });

  it("serializes metadata keys", () => {
    const req = buildCheckoutSession("sk", { ...base, metadata: { tier: "pro", userId: "u_1" } });
    expect(req.body).toContain("metadata%5Btier%5D=pro");
    expect(req.body).toContain("metadata%5BuserId%5D=u_1");
  });

  it("serializes customerEmail when set", () => {
    const req = buildCheckoutSession("sk", { ...base, customerEmail: "a@b.co" });
    expect(req.body).toContain("customer_email=a%40b.co");
  });

  it("rejects invalid applicationFeePercent", () => {
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: 150 }))
      .toThrow(/0\.\.=100/);
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: -1 }))
      .toThrow(/0\.\.=100/);
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: NaN }))
      .toThrow(/0\.\.=100/);
  });

  it("rejects missing required fields", () => {
    expect(() => buildCheckoutSession("", base)).toThrow(/apiKey/);
    expect(() => buildCheckoutSession("sk", { ...base, priceId: "" })).toThrow(/priceId/);
    expect(() => buildCheckoutSession("sk", { ...base, creatorAccountId: "" })).toThrow(/creatorAccountId/);
    expect(() => buildCheckoutSession("sk", { ...base, successUrl: "" })).toThrow(/successUrl/);
    expect(() => buildCheckoutSession("sk", { ...base, cancelUrl: "" })).toThrow(/cancelUrl/);
  });
});
