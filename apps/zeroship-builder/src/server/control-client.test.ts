"use server";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  loadTokens: vi.fn(),
  saveTokens: vi.fn(),
  refreshAccessToken: vi.fn(),
}));

vi.mock("./oauth-store.js", () => ({
  loadTokens: mocks.loadTokens,
  saveTokens: mocks.saveTokens,
}));

vi.mock("./oauth.js", () => ({
  refreshAccessToken: mocks.refreshAccessToken,
}));

import {
  ControlApiError,
  ControlClient,
  OauthExpiredError,
} from "./control-client";

const USER_ID = "usr_control_client_test";
const BASE_URL = "https://control.test";
const STORED_TOKENS = {
  access_token: "access-old",
  refresh_token: "refresh-old",
  expires_at: 4_102_444_800,
  scope: "apps:read apps:write",
};

describe("ControlClient", () => {
  beforeEach(() => {
    mocks.loadTokens.mockReset();
    mocks.saveTokens.mockReset();
    mocks.refreshAccessToken.mockReset();

    mocks.loadTokens.mockResolvedValue(STORED_TOKENS);
    mocks.saveTokens.mockResolvedValue(undefined);
    mocks.refreshAccessToken.mockResolvedValue({
      access_token: "access-new",
      refresh_token: "refresh-new",
      expires_in: 3600,
      scope: "apps:read apps:write apps:deploy",
      token_type: "Bearer",
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("attaches Authorization: Bearer header from oauth-store", async () => {
    const fetchMock = vi.fn(async () => Response.json([]));
    vi.stubGlobal("fetch", fetchMock);

    await newClient().listApps();

    expect(mocks.loadTokens).toHaveBeenCalledWith(USER_ID);
    const [, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(new Headers(init.headers).get("authorization")).toBe("Bearer access-old");
  });

  it("on 401, refreshes once and retries", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response("expired", { status: 401 }))
      .mockResolvedValueOnce(Response.json({ id: "app_one", name: "One" }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(newClient().createApp("One", "free")).resolves.toEqual({
      id: "app_one",
      name: "One",
    });

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(mocks.refreshAccessToken).toHaveBeenCalledTimes(1);
    expect(mocks.refreshAccessToken).toHaveBeenCalledWith("refresh-old");
    expect(mocks.saveTokens).toHaveBeenCalledTimes(1);
    expect(mocks.saveTokens).toHaveBeenCalledWith(USER_ID, {
      access_token: "access-new",
      refresh_token: "refresh-new",
      expires_at: expect.any(Number),
      scope: "apps:read apps:write apps:deploy",
    });

    const [, firstInit] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    const [, secondInit] = fetchMock.mock.calls[1] as unknown as [string, RequestInit];
    expect(new Headers(firstInit.headers).get("authorization")).toBe("Bearer access-old");
    expect(new Headers(secondInit.headers).get("authorization")).toBe("Bearer access-new");
  });

  it("on second 401 after refresh, throws OauthExpiredError", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(new Response("expired", { status: 401 }))
      .mockResolvedValueOnce(new Response("still expired", { status: 401 }));
    vi.stubGlobal("fetch", fetchMock);

    await expect(newClient().listApps()).rejects.toBeInstanceOf(OauthExpiredError);

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(mocks.refreshAccessToken).toHaveBeenCalledTimes(1);
    expect(mocks.saveTokens).toHaveBeenCalledTimes(1);
  });

  it("on refresh failure, throws OauthExpiredError", async () => {
    const fetchMock = vi.fn(async () => new Response("expired", { status: 401 }));
    vi.stubGlobal("fetch", fetchMock);
    mocks.refreshAccessToken.mockRejectedValueOnce(new Error("hydra down"));

    await expect(newClient().listApps()).rejects.toBeInstanceOf(OauthExpiredError);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(mocks.saveTokens).not.toHaveBeenCalled();
  });

  it("throws OauthExpiredError when no tokens stored", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    mocks.loadTokens.mockResolvedValueOnce(null);

    await expect(newClient().listApps()).rejects.toBeInstanceOf(OauthExpiredError);

    expect(fetchMock).not.toHaveBeenCalled();
    expect(mocks.refreshAccessToken).not.toHaveBeenCalled();
  });

  it("deploy posts raw bytes with application/x-zship content-type", async () => {
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
    expect(headers.get("authorization")).toBe("Bearer access-old");
    expect(headers.get("content-type")).toBe("application/x-zship");
    expect(init.body).toBe(bytes);
  });

  it.each([400, 403, 404])(
    "does NOT retry on %i",
    async (status) => {
      const fetchMock = vi.fn(async () => new Response("bad request", { status }));
      vi.stubGlobal("fetch", fetchMock);

      await expect(newClient().listApps()).rejects.toMatchObject({
        name: "ControlApiError",
        status,
      } satisfies Partial<ControlApiError>);

      expect(fetchMock).toHaveBeenCalledTimes(1);
      expect(mocks.refreshAccessToken).not.toHaveBeenCalled();
      expect(mocks.saveTokens).not.toHaveBeenCalled();
    },
  );
});

function newClient(): ControlClient {
  return new ControlClient({ userId: USER_ID, baseUrl: BASE_URL });
}
