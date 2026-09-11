import assert from "node:assert/strict";
import { setTimeout as sleep } from "node:timers/promises";

export type Row = Record<string, unknown>;
export type Capture = Record<string, unknown>;
export function object(value: unknown): Row {
  assert(value && typeof value === "object" && !Array.isArray(value), `Expected object: ${JSON.stringify(value)}`);
  return value as Row;
}
export function rows(value: unknown): Row[] {
  assert(Array.isArray(value), `Expected rows: ${JSON.stringify(value)}`);
  return value.map(object);
}
function id(value: unknown): string {
  const result = object(value).id;
  assert.equal(typeof result, "string", `Missing typed id: ${JSON.stringify(value)}`);
  return result as string;
}
export async function call(base: string, name: string, input: Row = {}): Promise<Row> {
  const response = await fetch(`${base}/__zeroship/v1/${name}`, {
    method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ json: input }), signal: AbortSignal.timeout(30_000),
  });
  const text = await response.text();
  const value = object(JSON.parse(text));
  assert("json" in value || "error" in value, `${name}: invalid response ${response.status}: ${text}`);
  return value;
}

export async function capture(base: string, run: string): Promise<Capture> {
  const out: Capture = {};
  const row = async (label: string, operation: string, input: Row = {}) => {
    assert(!(label in out), `Duplicate capture: ${label}`);
    const reply = await call(base, operation, input); out[label] = reply; return reply.json;
  };
  const seed = (label: string, name: string) => row(label, "users.seed", {
    email: `${name.toLowerCase()}-${run}@probe.test`, name, handle: `${name.toLowerCase()}_${run}`,
  });
  const alice = id(await seed("seedA", "Alice"));
  const bob = id(await seed("seedB", "Bob"));
  const tasks: unknown[] = [];
  for (const [index, [title, priority, userId]] of [
    ["buy milk", "low", alice], ["walk dog", undefined, alice], ["ship it", "high", alice],
    ["bob task", undefined, bob], ["read book", undefined, alice], ["pay bills", "high", alice],
  ].entries()) tasks.push(await row(`mkT${index + 1}`, "todos.create", { userId, title, ...(priority ? { priority } : {}) }));
  const first = id(tasks[0]);
  await row("orphan", "todos.create", { userId: "user_doesNotExist0000000", title: "orphan" });
  await row("orphanN", "todos.count", { userId: "user_doesNotExist0000000" });
  await row("dupEmail", "users.seed", { email: `alice-${run}@probe.test`, name: "Dup", handle: `dup_${run}` });
  await row("getT1", "todos.get", { id: first });
  await row("getNone", "todos.get", { id: "todo_0000000000000000000000" });
  await row("countA", "todos.count", { userId: alice });
  await row("list", "todos.list", { userId: alice });
  await row("withUser", "todos.listWithUser", { userId: alice });
  await row("pair", "users.getPair", { aId: alice, bId: bob });
  const page1 = object(await row("p1", "todos.listPage", { userId: alice, cursor: null, numItems: 2 }));
  const page2 = object(await row("p2", "todos.listPage", { userId: alice, cursor: page1.continueCursor, numItems: 2 }));
  await row("p3", "todos.listPage", { userId: alice, cursor: page2.continueCursor, numItems: 2 });
  for (const [key, page] of [["p1cur", page1], ["p2cur", page2]] as const) {
    assert.equal(typeof page.continueCursor, "string");
    out[key] = JSON.parse(Buffer.from(page.continueCursor as string, "base64").toString("utf8"));
  }
  await sleep(1_200);
  const updated = object(await row("setDone", "todos.setDone", { id: first, done: true }));
  await row("archive", "todos.archive", { id: first });
  await row("del", "todos.delete", { id: first });
  await row("getDel", "todos.get", { id: first });
  await row("listAfter", "todos.list", { userId: alice });
  const task = object(tasks[1]);
  out.tsrel = { digits: String(task.created_at).length, equal: task.created_at === task.updated_at };
  assert.equal(typeof updated.updated_at, "number"); assert.equal(typeof updated.created_at, "number");
  out.tsupd = { moved: (updated.updated_at as number) > (updated.created_at as number) };
  out.tsres = { distinct: new Set(tasks.map((task) => object(task).created_at)).size, inserted: tasks.length };
  const tx = id(await seed("seedTx", "Tx"));
  for (const [label, operation, tag] of [["txCommit", "txCommit", "c"], ["txRoll", "txRollback", "r"], ["txNest", "txNested", "n"]]) {
    await row(label, `todos.${operation}`, { userId: tx, tag: `${tag}${run}` });
  }
  for (const [index, [label, level]] of [["txIsoNone", null], ["txIsoSer", "serializable"], ["txIsoRR", "repeatableRead"], ["txIsoBad", "snapshot"]].entries()) {
    await row(label as string, "todos.txIsolation", { userId: tx, tag: `i${index}${run}`, level });
  }
  for (const [label, tag, levels] of [["txD9", "d9", 9], ["txD10", "da", 10]] as const) {
    await row(label, "todos.txDepth", { userId: tx, tag: `${tag}${run}`, levels });
  }
  await row("txTotal", "todos.count", { userId: tx });
  const cx = id(await seed("cxSeed", "Cx"));
  for (const [label, operation, tag] of [["cxPar", "txParallel", "p"], ["cxOvl", "txOverlap", "o"], ["cxPlain", "txPlainWrite", "w"]]) {
    await row(label, `todos.${operation}`, { userId: cx, tag: `${tag}${run}`, holdMs: 400 });
  }
  await row("cxTotal", "todos.count", { userId: cx });
  return out;
}

export async function captureScopes(base: string, run: string, out: Capture) {
  const seeded = await call(base, "users.seed", { email: `bx-${run}@probe.test`, name: "Bx", handle: `bx_${run}` });
  out.bxSeed = seeded;
  const userId = id(seeded.json);
  out.txBranch = await call(base, "todos.txBranchWrites", { userId, tag: `b${run}` });
  out.txOrphan = await call(base, "todos.txOrphanedWrite", { userId, tag: `r${run}`, holdMs: 300 });
  out.bxTotal = await call(base, "todos.count", { userId });
}

export async function race(base: string) {
  const run = `${Date.now()}${Math.floor(Math.random() * 1e6)}`;
  const userId = id((await call(base, "users.seed", { email: `race-${run}@probe.test`, name: "Race", handle: `race_${run}` })).json);
  const results = [];
  for (let index = 0; index < 16; index++) {
    const tag = `race-${run}-${index}`;
    const fire = async (delay: number) => {
      await sleep(delay);
      return object((await call(base, "todos.txRaceStep", { userId, tag, holdMs: 400, level: "serializable" })).json);
    };
    const [a, b] = await Promise.all([fire(0), fire(100)]);
    for (const value of [a, b]) {
      assert.equal(typeof value.t0, "number"); assert.equal(typeof value.t1, "number");
    }
    const count = (await call(base, "todos.countTitle", { userId, title: tag })).json;
    const successes = [a, b].filter((value) => !value.threw && !value.error).length;
    results.push({ a, b, count, successes, overlap: (b.t0 as number) < (a.t1 as number) && (a.t0 as number) < (b.t1 as number) });
  }
  return results;
}

export function normalize(capture: Capture): Capture {
  const ids = new Map<string, string>();
  const walk = (value: unknown, key = ""): unknown => {
    if (typeof value === "number" && ["created_at", "updated_at", "deleted_at"].includes(key)) return "<timestamp>";
    if (typeof value === "string") {
      if (key === "request_id") return "<request>";
      if (key === "continueCursor") return "<cursor>";
      return value.replace(/\b(?:user|todo)_[0-9A-Za-z]+\b/g, (id) => {
        if (!ids.has(id)) ids.set(id, `<id-${ids.size}>`);
        return ids.get(id)!;
      });
    }
    if (Array.isArray(value)) return value.map((v) => walk(v));
    if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => [k, walk(v, k)]));
    return value;
  };
  return walk(capture) as Capture;
}
