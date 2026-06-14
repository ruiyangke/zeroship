import { describe, it, expect } from "vitest";
import { verifyWebhook } from "../src/webhook";
import { signWebhookForTest } from "../src/testing";

const SECRET = "whsec_test_EXAMPLE";
const NOW = 1_700_000_000;

// Pinned cross-validation fixture — these MUST match the constants in
// crates/control/src/stripe_handlers.rs (verification_tests). If
// either side's HMAC implementation drifts, both suites fail at once.
const CROSS_SECRET = "whsec_cross_validation_FIXTURE_v1";
const CROSS_BODY = '{"id":"evt_cross","type":"invoice.paid","created":1700000000}';
const CROSS_TIMESTAMP = 1_700_000_000;
const CROSS_EXPECTED_HEX = "3a9a1b18f1a3f804c7323a527d3f8588d54cac8e89d3c8572be160ebc904f765";

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

  it("cross-validates with the Rust verification_tests fixture", async () => {
    // Same secret + body + timestamp as the Rust fixture in
    // crates/control/src/stripe_handlers.rs::verification_tests. If the
    // hex below diverges, EITHER the Rust HMAC or the TS HMAC has
    // drifted — investigate before changing the constant.
    const header = await signWebhookForTest(CROSS_BODY, CROSS_SECRET, CROSS_TIMESTAMP);
    const v1 = header.split(",").find((p) => p.startsWith("v1="))!.slice("v1=".length);
    expect(v1).toBe(CROSS_EXPECTED_HEX);

    // And confirm verifyWebhook accepts the pinned header end-to-end.
    const result = await verifyWebhook(
      CROSS_BODY, header, CROSS_SECRET, { now: () => CROSS_TIMESTAMP },
    );
    expect(result).toEqual({ valid: true, timestamp: CROSS_TIMESTAMP });
  });
});
