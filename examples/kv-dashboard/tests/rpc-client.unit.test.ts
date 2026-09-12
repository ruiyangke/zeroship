import { afterEach, expect, test, vi } from "vitest";
import { listKeys, rpc } from "./rpc";
import { targets } from "./targets";

afterEach(() => vi.unstubAllGlobals());

test("missing fixture targets cannot silently use an existing server", () => {
  expect(() => targets()).toThrow("did not provide test targets");
});

test("HTTP failures and missing result envelopes cannot look like a successful smoke", async () => {
  for (const response of [new Response("unavailable", { status: 503 }), Response.json({ error: "rejected" }), Response.json({ json: null })]) {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));
    await expect(rpc("http://fixture", "kv.snapshot")).rejects.toThrow();
  }
});

test("pagination follows cursors through empty pages and rejects loops", async () => {
  const fetch = vi.fn()
    .mockResolvedValueOnce(Response.json({ json: { keys: [], cursor: "next" } }))
    .mockResolvedValueOnce(Response.json({ json: { keys: ["session:a"], cursor: null } }));
  vi.stubGlobal("fetch", fetch);
  expect(await listKeys("http://fixture", "session:")).toEqual(["session:a"]);
  expect(JSON.parse(fetch.mock.calls[1][1].body).json.cursor).toBe("next");
  vi.stubGlobal("fetch", vi.fn().mockImplementation(() => Promise.resolve(Response.json({ json: { keys: [], cursor: "loop" } }))));
  await expect(listKeys("http://fixture")).rejects.toThrow("repeated a cursor");
});
