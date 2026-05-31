"use server";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Identity now comes from the PLATFORM session via the `zeroship`
// module's `currentUser()` (the gateway-verified `ZeroShip-User`), NOT a
// bespoke OAuth-RP cookie/refresh loop. Mock it so the unit can drive the
// "authenticated creator" vs "no user" branches.
const mocks = vi.hoisted(() => ({
  currentUser: vi.fn(),
}));

vi.mock("zeroship", () => ({
  currentUser: mocks.currentUser,
}));

import {
  ControlApiError,
  ControlClient,
  NotAuthenticatedError,
  getControlClient,
} from "./control-client";

const USER_ID = "usr_control_client_test";
const BASE_URL = "https://control.test";
const SERVICE_TOKEN = "pat_service_credential_abc123";

describe("ControlClient (env-var control service credential)", () => {
  beforeEach(() => {
    mocks.currentUser.mockReset();
    mocks.currentUser.mockReturnValue({ id: USER_ID, email: "c@example.com" });
    // The control service credential is a SERVER-ONLY app env var.
    process.env.ZS_CONTROL_SERVICE_TOKEN = SERVICE_TOKEN;
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    delete process.env.ZS_CONTROL_SERVICE_TOKEN;
  });

  it("authenticates with the ZS_CONTROL_SERVICE_TOKEN bearer, not a user token", async () => {
    const fetchMock = vi.fn(async () => Response.json([]));
    vi.stubGlobal("fetch", fetchMock);

    await newClient().listApps();

    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe(`${BASE_URL}/api/apps`);
    // The bearer is the server-only service credential — control's
    // AuthzGuard bearer path verifies it as a control PAT.
    expect(new Headers(init.headers).get("authorization")).toBe(
      `Bearer ${SERVICE_TOKEN}`,
    );
  });

  it("threads the acting creator's id to the control plane (attribution-only until full-R4)", async () => {
    const fetchMock = vi.fn(async () => Response.json([]));
    vi.stubGlobal("fetch", fetchMock);

    await newClient().listApps();

    const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(new Headers(init.headers).get("zeroship-acting-user")).toBe(USER_ID);
  });

  it("throws (loudly) when the service credential is not configured", async () => {
    delete process.env.ZS_CONTROL_SERVICE_TOKEN;
    const fetchMock = vi.fn(async () => Response.json([]));
    vi.stubGlobal("fetch", fetchMock);

    await expect(newClient().listApps()).rejects.toThrow(
      /ZS_CONTROL_SERVICE_TOKEN is not set/,
    );
    // A missing credential must NOT fall back to an unauthenticated call.
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("deploy posts the artifact with application/x-zship content-type", async () => {
    const fetchMock = vi.fn(async () => Response.json({ deploy_hash: "sha256:abc" }));
    vi.stubGlobal("fetch", fetchMock);
    const bytes = new Uint8Array([1, 2, 3, 4]);

    await expect(newClient().deploy("app_one", bytes)).resolves.toEqual({
      deploy_hash: "sha256:abc",
    });

    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    const headers = new Headers(init.headers);
    expect(url).toBe(`${BASE_URL}/api/apps/app_one/deploy`);
    expect(init.method).toBe("POST");
    expect(headers.get("authorization")).toBe(`Bearer ${SERVICE_TOKEN}`);
    expect(headers.get("content-type")).toBe("application/x-zship");
  });

  it.each([400, 403, 404, 503])(
    "maps control %i failures to ControlApiError",
    async (status) => {
      const fetchMock = vi.fn(async () => new Response("bad request", { status }));
      vi.stubGlobal("fetch", fetchMock);
      vi.spyOn(console, "error").mockImplementation(() => {});

      await expect(newClient().listApps()).rejects.toMatchObject({
        name: "ControlApiError",
        status,
      } satisfies Partial<ControlApiError>);
    },
  );

  it("does not expose upstream response bodies in ControlApiError.message", async () => {
    const fetchMock = vi.fn(async () => new Response(
      "{\"error\":\"internal\",\"detail\":\"postgres://internal\"}",
      { status: 503 },
    ));
    vi.stubGlobal("fetch", fetchMock);
    vi.spyOn(console, "error").mockImplementation(() => {});

    await expect(newClient().getAppLogs("app_one")).rejects.toMatchObject({
      name: "ControlApiError",
      status: 503,
      message: "control request failed",
    } satisfies Partial<ControlApiError>);

    await expect(newClient().getAppLogs("app_one")).rejects.not.toThrow(
      "postgres://internal",
    );
  });

  describe("getControlClient — platform identity", () => {
    it("resolves the creator from currentUser()", async () => {
      const fetchMock = vi.fn(async () => Response.json([]));
      vi.stubGlobal("fetch", fetchMock);

      await getControlClient().listApps();

      const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
      expect(new Headers(init.headers).get("zeroship-acting-user")).toBe(USER_ID);
    });

    it("throws NotAuthenticatedError when there is no platform user", () => {
      mocks.currentUser.mockReturnValue(null);
      expect(() => getControlClient()).toThrow(NotAuthenticatedError);
    });

    it("throws NotAuthenticatedError when currentUser() throws (outside a request)", () => {
      mocks.currentUser.mockImplementation(() => {
        throw new Error("called outside a request handler");
      });
      expect(() => getControlClient()).toThrow(NotAuthenticatedError);
    });
  });
});

function newClient(): ControlClient {
  return new ControlClient({ userId: USER_ID, baseUrl: BASE_URL });
}
