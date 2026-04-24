import { describe, it, expect } from "vitest";
import { verifyWebhook } from "../src/webhook";
import { signWebhookForTest } from "../src/testing";
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
    expect(result).toEqual({ valid: false, reason: "missing v1" });
  });

  it("rejects a header with a non-numeric timestamp", async () => {
    const body = '{"x":1}';
    const result = await verifyWebhook(body, "t=oops,v1=abcd", SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "bad t" });
  });

  it("rejects an empty header", async () => {
    const result = await verifyWebhook('{"x":1}', "", SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "missing t" });
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

  it("accepts the valid v1 when multiple are present (rotation window)", async () => {
    // During secret rotation Stripe sends one v1 per active secret.
    // Our parser must try ALL of them, not just the last.
    const body = '{"rotate":true}';
    const valid = await signWebhookForTest(body, SECRET, NOW);
    // Prepend a v1 signed with a different (old) secret; current must match.
    const oldSig = await signWebhookForTest(body, "old-secret", NOW);
    const oldV1 = oldSig.split(",").find((p) => p.startsWith("v1="))!;
    const header = `${valid},${oldV1}`;
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: true, timestamp: NOW });
  });

  it("rejects when all v1 entries are wrong", async () => {
    const body = '{"all":"wrong"}';
    const a = await signWebhookForTest(body, "secret-A", NOW);
    const b = await signWebhookForTest(body, "secret-B", NOW);
    const header = `${a.split(",")[0]},${a.split(",")[1]},${b.split(",")[1]}`;
    const result = await verifyWebhook(body, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "signature mismatch" });
  });

  it("rejects when secret is empty", async () => {
    const result = await verifyWebhook("{}", "t=1,v1=abc", "", { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "empty signing secret" });
  });

  it("accepts Uint8Array body", async () => {
    const bytes = new TextEncoder().encode('{"u8":true}');
    const header = await signWebhookForTest(bytes, SECRET, NOW);
    const result = await verifyWebhook(bytes, header, SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: true, timestamp: NOW });
  });

  it("fails on floating-point t", async () => {
    const result = await verifyWebhook('{"x":1}', "t=1.5,v1=abc", SECRET, { now: () => NOW });
    expect(result).toEqual({ valid: false, reason: "bad t" });
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

  it("sets metadata on BOTH session and subscription_data", () => {
    const req = buildCheckoutSession("sk", {
      ...base,
      metadata: { creator_id: "c1", tier: "pro" },
    });
    // Session-level metadata (visible on payment_intent).
    expect(req.body).toContain("metadata%5Bcreator_id%5D=c1");
    expect(req.body).toContain("metadata%5Btier%5D=pro");
    // Subscription-level metadata (visible on invoice.paid).
    expect(req.body).toContain("subscription_data%5Bmetadata%5D%5Bcreator_id%5D=c1");
    expect(req.body).toContain("subscription_data%5Bmetadata%5D%5Btier%5D=pro");
  });

  it("rejects metadata keys outside Stripe's allowed charset", () => {
    expect(() => buildCheckoutSession("sk", { ...base, metadata: { "bad-key": "v" } }))
      .toThrow(/metadata key/);
    expect(() => buildCheckoutSession("sk", { ...base, metadata: { "": "v" } }))
      .toThrow(/metadata key/);
    expect(() => buildCheckoutSession("sk", { ...base, metadata: { [`k${"a".repeat(41)}`]: "v" } }))
      .toThrow(/metadata key/);
  });

  it("serializes customerEmail when set", () => {
    const req = buildCheckoutSession("sk", { ...base, customerEmail: "a@b.co" });
    expect(req.body).toContain("customer_email=a%40b.co");
  });

  it("pins stripe-version header when set", () => {
    const req = buildCheckoutSession("sk", { ...base, stripeVersion: "2024-06-20" });
    expect(req.headers["stripe-version"]).toBe("2024-06-20");
  });

  it("omits stripe-version header when unset", () => {
    const req = buildCheckoutSession("sk", base);
    expect(req.headers["stripe-version"]).toBeUndefined();
  });

  it("rejects invalid applicationFeePercent", () => {
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: 150 }))
      .toThrow(/\[0, 100\]/);
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: -1 }))
      .toThrow(/\[0, 100\]/);
    expect(() => buildCheckoutSession("sk", { ...base, applicationFeePercent: NaN }))
      .toThrow(/\[0, 100\]/);
  });

  it("rejects non-http(s) redirect URLs", () => {
    expect(() => buildCheckoutSession("sk", { ...base, successUrl: "javascript:alert(1)" }))
      .toThrow(/valid http/);
    expect(() => buildCheckoutSession("sk", { ...base, cancelUrl: "./relative" }))
      .toThrow(/valid http/);
    expect(() => buildCheckoutSession("sk", { ...base, successUrl: "file:///etc/passwd" }))
      .toThrow(/valid http/);
  });

  it("rejects missing required fields", () => {
    expect(() => buildCheckoutSession("", base)).toThrow(/apiKey/);
    expect(() => buildCheckoutSession("sk", { ...base, priceId: "" })).toThrow(/priceId/);
    expect(() => buildCheckoutSession("sk", { ...base, creatorAccountId: "" })).toThrow(/creatorAccountId/);
    expect(() => buildCheckoutSession("sk", { ...base, successUrl: "" })).toThrow(/successUrl/);
    expect(() => buildCheckoutSession("sk", { ...base, cancelUrl: "" })).toThrow(/cancelUrl/);
  });
});
