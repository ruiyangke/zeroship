import { describe, it, expect } from "vitest";

import {
  createPaymentsClient,
  PaymentsError,
  type PaymentsClientOptions,
} from "../src/connect";

const CREATOR = "usr_creator_1";

/**
 * Build a client whose `fetch` records every outgoing request and returns a
 * canned response. The recorded `Request` lets each test assert the EXACT wire
 * shape — method, URL, headers, and (crucially) the JSON body.
 */
function clientWith(
  responder: (req: Request) => Response | Promise<Response>,
  extra: Partial<PaymentsClientOptions> = {},
) {
  const requests: Request[] = [];
  const client = createPaymentsClient({
    baseUrl: "http://control.local",
    creatorId: CREATOR,
    auth: "master-key",
    fetch: async (input, init) => {
      const req = new Request(input, init);
      requests.push(req);
      return responder(req);
    },
    ...extra,
  });
  return { client, requests };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("checkout", () => {
  it("posts ONLY business params — no fee field on the wire", async () => {
    const { client, requests } = clientWith(() =>
      json({
        payment_intent_id: "pi_123",
        client_secret: "pi_123_secret_abc",
        application_fee_cents: 150,
      }),
    );

    const result = await client.checkout({
      amountCents: 1000,
      currency: "usd",
      cartId: "cart_42",
    });

    expect(requests).toHaveLength(1);
    const req = requests[0]!;
    expect(req.method).toBe("POST");
    expect(new URL(req.url).pathname).toBe(
      `/api/creators/${CREATOR}/connect/checkout`,
    );
    expect(req.headers.get("authorization")).toBe("Bearer master-key");
    expect(req.headers.get("content-type")).toBe("application/json");

    const sent = (await req.json()) as Record<string, unknown>;
    // Exactly the business params — nothing more.
    expect(sent).toEqual({
      amount_cents: 1000,
      currency: "usd",
      cart_id: "cart_42",
    });
    // Belt-and-braces: assert NO fee-shaped key leaked onto the wire.
    const keys = Object.keys(sent);
    for (const k of keys) {
      expect(k.toLowerCase()).not.toContain("fee");
      expect(k.toLowerCase()).not.toContain("percent");
      expect(k.toLowerCase()).not.toContain("application");
    }

    // The server-stamped fee is surfaced read-only.
    expect(result).toEqual({
      paymentIntentId: "pi_123",
      clientSecret: "pi_123_secret_abc",
      applicationFeeCents: 150,
    });
  });

  it("includes an optional description but still no fee", async () => {
    const { client, requests } = clientWith(() =>
      json({
        payment_intent_id: "pi_9",
        client_secret: "secret_9",
        application_fee_cents: 75,
      }),
    );

    await client.checkout({
      amountCents: 500,
      currency: "eur",
      cartId: "cart_x",
      description: "Pro plan",
    });

    const sent = (await requests[0]!.json()) as Record<string, unknown>;
    expect(sent).toEqual({
      amount_cents: 500,
      currency: "eur",
      cart_id: "cart_x",
      description: "Pro plan",
    });
  });

  it("drops any extra property a caller forces past the types (no fee path)", async () => {
    const { client, requests } = clientWith(() =>
      json({
        payment_intent_id: "pi_1",
        client_secret: "s",
        application_fee_cents: 0,
      }),
    );

    // A determined caller bypasses the compile-time types with a cast and
    // tries to smuggle a fee. The SDK builds the body EXPLICITLY, so the
    // forged field is dropped — it never reaches the wire.
    await client.checkout({
      amountCents: 2000,
      currency: "usd",
      cartId: "cart_evil",
      applicationFeePercent: 0,
      application_fee_amount: 0,
      fee: 0,
    } as unknown as Parameters<typeof client.checkout>[0]);

    const sent = (await requests[0]!.json()) as Record<string, unknown>;
    expect(sent).toEqual({
      amount_cents: 2000,
      currency: "usd",
      cart_id: "cart_evil",
    });
    expect("applicationFeePercent" in sent).toBe(false);
    expect("application_fee_amount" in sent).toBe(false);
    expect("fee" in sent).toBe(false);
  });

  it("validates business inputs locally before hitting the network", async () => {
    const { client, requests } = clientWith(() => json({}, 200));

    await expect(
      client.checkout({ amountCents: 0, currency: "usd", cartId: "c" }),
    ).rejects.toThrow(/amountCents/);
    await expect(
      client.checkout({ amountCents: 100, currency: "", cartId: "c" }),
    ).rejects.toThrow(/currency/);
    await expect(
      client.checkout({ amountCents: 100, currency: "usd", cartId: "  " }),
    ).rejects.toThrow(/cartId/);

    expect(requests).toHaveLength(0);
  });

  it("maps a 400 (empty cart) server error to a PaymentsError", async () => {
    const { client } = clientWith(() =>
      json({ error: "cart_id is required" }, 400),
    );

    await expect(
      client.checkout({ amountCents: 100, currency: "usd", cartId: "x" }),
    ).rejects.toMatchObject({
      name: "ControlError",
      status: 400,
      message: "cart_id is required",
    });
  });

  it("maps a 400 (account not ready) server error to a PaymentsError", async () => {
    const { client } = clientWith(() =>
      json({ error: "creator stripe account not ready (complete onboarding)" }, 400),
    );

    try {
      await client.checkout({ amountCents: 100, currency: "usd", cartId: "x" });
      throw new Error("expected checkout to throw");
    } catch (e) {
      expect(e).toBeInstanceOf(PaymentsError);
      expect((e as PaymentsError).status).toBe(400);
    }
  });

  it("maps a 400 (bad currency) server error to a PaymentsError", async () => {
    const { client } = clientWith(() =>
      json({ error: "currency must be a 3-letter ISO code (lowercase)" }, 400),
    );

    await expect(
      client.checkout({ amountCents: 100, currency: "US", cartId: "x" }),
    ).rejects.toMatchObject({ status: 400 });
  });
});

describe("startOnboarding", () => {
  it("POSTs to the onboard endpoint and returns the hosted URL", async () => {
    const { client, requests } = clientWith(() =>
      json({
        url: "https://connect.stripe.com/setup/s/abc123",
        account_id: "acct_xyz",
      }),
    );

    const result = await client.startOnboarding();

    expect(requests).toHaveLength(1);
    const req = requests[0]!;
    expect(req.method).toBe("POST");
    expect(new URL(req.url).pathname).toBe(
      `/api/creators/${CREATOR}/stripe/onboard`,
    );
    expect(result).toEqual({
      url: "https://connect.stripe.com/setup/s/abc123",
      accountId: "acct_xyz",
    });
  });

  it("maps a 404 (creator not found) to a PaymentsError", async () => {
    const { client } = clientWith(() => json({ error: "creator not found" }, 404));

    await expect(client.startOnboarding()).rejects.toMatchObject({
      name: "ControlError",
      status: 404,
    });
  });
});

describe("createPaymentsClient", () => {
  it("requires a creatorId", () => {
    expect(() =>
      createPaymentsClient({
        baseUrl: "http://control.local",
      } as unknown as PaymentsClientOptions),
    ).toThrow(/creatorId/);
  });

  it("forwards a cookie provider (server-side session forwarding)", async () => {
    const seen: Array<string | null> = [];
    const { client } = clientWith(
      (req) => {
        seen.push(req.headers.get("cookie"));
        return json({
          payment_intent_id: "pi",
          client_secret: "s",
          application_fee_cents: 0,
        });
      },
      { auth: undefined, cookie: () => "session=tok" },
    );

    await client.checkout({ amountCents: 100, currency: "usd", cartId: "c" });
    expect(seen).toEqual(["session=tok"]);
  });
});
