#!/usr/bin/env node
//
// Probe a DEPLOYED db-todos for the one property a page load cannot show:
// that `todos.subscribe` streams a write made by a different connection.
//
//   node examples/db-todos/scripts/probe-live.mjs https://db-todos.zeroship.co
//   ZEROSHIP_E2E_BASE_URL=https://db-todos.zeroship.co node .../probe-live.mjs
//
// WHY THIS EXISTS, 2026-08-12. The deployed HTML is byte-identical whether or
// not the CDC path works, so `curl https://.../ -> 200` says nothing about the
// subscription. The Playwright suite does assert it, but needs a browser and a
// pnpm install. This probe needs only node's built-in fetch.
//
// It is deliberately the same contract the browser test drives:
// POST /__zeroship/v1/<id>, `accept: text/event-stream` for the subscription.
//
// SIDE EFFECT: it creates one todo on the target, titled `probe-live <ts>`.
// Point it at a throwaway deployment if that matters.
//
// WHAT IT DOES NOT COVER: only the subscribe path. It does not exercise the
// UI, auth redirects, asset serving, or any mutation other than create. A
// green run means the CDC fanout reached one subscriber on one node; with
// several worker nodes behind the gateway it does not prove every node is
// wired, because the write and the stream may land on the same one.

const base = (process.argv[2] ?? process.env.ZEROSHIP_E2E_BASE_URL ?? "").replace(/\/$/, "");
if (!base) {
  console.error("usage: probe-live.mjs <base-url>   (or set ZEROSHIP_E2E_BASE_URL)");
  process.exit(2);
}

const STREAM_DEADLINE_MS = 15_000;
const fail = (msg) => {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
};

const rpc = async (id, input, accept = "application/json", signal) => {
  const res = await fetch(`${base}/__zeroship/v1/${id}`, {
    method: "POST",
    headers: { accept, "content-type": "application/json" },
    body: JSON.stringify(input),
    signal,
  });
  return res;
};

const unwrap = (payload) => {
  // The transformer envelope keys the value under `json`, NOT `result`.
  // Matches e2e/ui.spec.ts; guessing `result` here made the probe report a
  // missing id while the server had in fact answered correctly.
  if (payload && typeof payload === "object" && "error" in payload && payload.error) {
    throw new Error(`rpc error: ${JSON.stringify(payload.error)}`);
  }
  if (payload && typeof payload === "object" && "json" in payload) return payload.json;
  return payload;
};

console.log(`target: ${base}`);

// 1. Identify. Also proves plain JSON RPC works before blaming the stream.
const userRes = await rpc("users.public", {});
if (!userRes.ok) fail(`users.public -> HTTP ${userRes.status}`);
const user = unwrap(await userRes.json());
if (!user?.id) fail(`users.public returned no id: ${JSON.stringify(user)}`);
console.log(`ok  users.public -> ${user.id}`);

// 2. Open the subscription.
const controller = new AbortController();
const streamRes = await rpc("todos.subscribe", { userId: user.id }, "text/event-stream", controller.signal);
if (!streamRes.ok) fail(`todos.subscribe -> HTTP ${streamRes.status}`);
const ctype = streamRes.headers.get("content-type") ?? "";
if (!ctype.includes("text/event-stream")) {
  fail(`todos.subscribe content-type was ${ctype || "(absent)"}, not text/event-stream`);
}
if (!streamRes.body) fail("todos.subscribe returned no body");
console.log(`ok  todos.subscribe -> ${streamRes.status} ${ctype}`);

// 3. Collect frames in the background.
const lines = [];
let bytes = 0;
const reader = streamRes.body.getReader();
const decoder = new TextDecoder();
const pump = (async () => {
  let buffered = "";
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value?.length ?? 0;
      buffered += decoder.decode(value, { stream: true });
      for (;;) {
        const idx = buffered.indexOf("\n");
        if (idx < 0) break;
        const line = buffered.slice(0, idx).trim();
        buffered = buffered.slice(idx + 1);
        if (line) lines.push(line);
      }
    }
  } catch {
    // Aborting the controller lands here; the assertions below own the verdict.
  }
})();

const waitFor = async (predicate, ms, what) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, 100));
  }
  fail(`timed out after ${ms}ms waiting for ${what}. Frames seen: ${lines.length}, bytes: ${bytes}`);
  return false;
};

// The initial snapshot must arrive first. Without this the write assertion
// could pass on a stream that was never actually established.
await waitFor(() => bytes > 0, STREAM_DEADLINE_MS, "the initial snapshot");
console.log(`ok  initial snapshot (${bytes} bytes, ${lines.length} frames)`);

// 4. Write from a SEPARATE request, which is the whole point: a same-isolate
// echo would satisfy a naive test, and that was the shape of the original bug.
const title = `probe-live ${new Date().toISOString()}`;
const createRes = await rpc("todos.create", { userId: user.id, title, priority: "low" });
if (!createRes.ok) fail(`todos.create -> HTTP ${createRes.status}`);
console.log(`ok  todos.create "${title}"`);

await waitFor(() => lines.some((l) => l.includes(title)), STREAM_DEADLINE_MS, "the written todo on the stream");

controller.abort();
await reader.cancel().catch(() => undefined);
await pump;

const matched = lines.filter((l) => l.includes(title));
if (matched.length !== 1) {
  fail(`expected exactly 1 frame carrying the write, saw ${matched.length}`);
}
console.log(`ok  stream carried the write (1 frame)`);
console.log("PASS: the deployed subscription carries a cross-connection write");
