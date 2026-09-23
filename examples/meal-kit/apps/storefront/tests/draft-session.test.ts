import { afterEach, expect, test, vi } from "vitest";
import { draftLifetimeMs } from "@gather/meal-kit/draft-domain";
const storage = vi.hoisted(() => new Map<string, unknown>());
vi.mock("@zeroship/kv", () => ({
  kv: {
    get: async (key: string) => ({ data: storage.get(key) ?? null }),
    setIfAbsent: async (key: string, value: unknown) => {
      if (!storage.has(key)) storage.set(key, value);
      return { data: true };
    },
  },
}));
vi.mock("@gather/meal-kit/server/core", () => ({
  must: (result: { data: unknown }) => result.data,
}));
import { draftSessionFetch, draftVisitor } from "../src/lib/draft-session";

const init = (headers: Record<string, string> = {}) =>
  new Request("https://gather.example/api/draft-session", {
    method: "POST",
    headers: { "X-Gather-Session": "init", ...headers },
  });
afterEach(() => vi.useRealTimers());
test("draft cookies are host-bound, signed, expiring and paired with a separate CSRF proof", async () => {
  const response = await draftSessionFetch(init());
  const cookie = response.headers.get("set-cookie")!;
  expect(cookie).toMatch(/^__Host-gather-draft=/);
  expect(cookie).toContain("HttpOnly; SameSite=Lax");
  expect(cookie).toContain("; Secure");
  expect(cookie).not.toContain("Domain=");
  const { csrf } = await response.json();
  const header = cookie.split(";")[0];
  const visitor = await draftVisitor(init({ cookie: header }), csrf);
  expect(visitor).toMatch(/^visitor:/);
  await expect(draftVisitor(init(), csrf)).rejects.toMatchObject({
    code: "DRAFT_SESSION",
  });
  await expect(
    draftVisitor(init({ cookie: header + "; " + header }), csrf),
  ).rejects.toMatchObject({ code: "DRAFT_SESSION" });
  await expect(
    draftVisitor(init({ cookie: header.slice(0, -4) + "AAAA" }), csrf),
  ).rejects.toMatchObject({ code: "DRAFT_SESSION" });
  const next = await draftSessionFetch(init());
  const other = next.headers.get("set-cookie")!.split(";")[0];
  await expect(
    draftVisitor(init({ cookie: other }), csrf),
  ).rejects.toMatchObject({ code: "DRAFT_SESSION" });
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(Date.now() + draftLifetimeMs + 1);
  await expect(
    draftVisitor(init({ cookie: header }), csrf),
  ).rejects.toMatchObject({ code: "DRAFT_SESSION" });
});
test("the persisted signing key survives module reload and session setup rejects cross-origin browser requests", async () => {
  const response = await draftSessionFetch(init());
  const cookie = response.headers.get("set-cookie")!.split(";")[0];
  const { csrf } = await response.json();
  const visitor = await draftVisitor(init({ cookie }), csrf);
  vi.resetModules();
  const reloaded = await import("../src/lib/draft-session");
  expect(await reloaded.draftVisitor(init({ cookie }), csrf)).toBe(visitor);
  for (const site of ["cross-site", "same-site"])
    expect(
      (await reloaded.draftSessionFetch(init({ "Sec-Fetch-Site": site })))
        .status,
    ).toBe(403);
  expect(
    (
      await reloaded.draftSessionFetch(
        new Request("https://gather.example/api/draft-session", {
          method: "POST",
        }),
      )
    ).status,
  ).toBe(403);
});
