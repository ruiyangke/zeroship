import { isDeepStrictEqual } from "node:util";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createServer } from "node:http";
import { expect, inject, test } from "vitest";
import { assertDeployed } from "./expectations";
import { call, capture, captureScopes, normalize, object, race, rows, type Capture } from "./rpc";
import { targets } from "./targets";

test("the database contract holds through local and deployed app requests", async () => {
  const captures: Capture[] = [];
  for (const target of targets()) {
    const run = "acceptance";
    const captured = await capture(target.apiUrl, run);
    const races = await race(target.apiUrl);
    await captureScopes(target.apiUrl, run, captured);
    await writeFile(join(inject("databaseArtifacts"), `${target.name}-capture.json`), JSON.stringify({ captured, races }, null, 2));
    expect(Object.keys(captured)).toHaveLength(50);
    expect(races).toHaveLength(16);
    expect(races.some((result) => result.overlap), "the requests must actually overlap").toBe(true);
    for (const result of races) expect.soft(result.count, `${target.name}: committed rows must match reported successes`).toBe(result.successes);
    if (target.name === "postgres") {
      assertDeployed(captured, run);
      const result = (name: string) => object(captured[name]).json;
      const inserted = object(result("mkT2"));
      for (const field of ["created_at", "updated_at", "created_by", "updated_by", "version", "deleted_at"]) expect(inserted).toHaveProperty(field);
      const listed = rows(result("list"));
      expect(listed[0].title).toBe("pay bills");
      const paged = ["p1", "p2", "p3"].flatMap((name) => rows(object(result(name)).page));
      expect(paged).toHaveLength(5);
      expect(new Set(paged.map((row) => row.id)).size).toBe(paged.length);
      expect(paged.map((row) => row.id).sort()).toEqual(listed.map((row) => row.id).sort());
      expect(captured.tsupd).toEqual({ moved: true });
      expect(captured.tsres).toEqual({ distinct: 6, inserted: 6 });
      const statements = await readFile(join(inject("databaseArtifacts"), "postgres.log"), "utf8");
      expect(statements.match(/LOG:/g)?.length).toBeGreaterThanOrEqual(50);
      for (const sql of ["BEGIN ISOLATION LEVEL SERIALIZABLE", "BEGIN ISOLATION LEVEL REPEATABLE READ", "SAVEPOINT zs_sp_1", "ROLLBACK TO SAVEPOINT zs_sp_1", "SAVEPOINT zs_sp_8"]) {
        expect(statements).toContain(sql);
      }
      expect(statements).not.toContain("BEGIN ISOLATION LEVEL SNAPSHOT");
    }
    captures.push(normalize(captured));
  }
  expect(captures).toHaveLength(2);
  const divergences = Object.keys(captures[0]).filter((key) => !isDeepStrictEqual(captures[0][key], captures[1][key])).sort();
  // Keep the old suite's named findings visible until the underlying behavior
  // is measured; unrelated differences always fail the comparison.
  const required = ["cxPlain", "cxTotal", "tsres"];
  const tolerated = ["txBranch", "txOrphan"];
  expect(divergences.filter((name) => !required.includes(name) && !tolerated.includes(name))).toEqual([]);
  expect(divergences).toEqual(expect.arrayContaining(required));
});

test("invalid creator input fails without writing rows", async () => {
  for (const target of targets()) {
    const seeded = await call(target.apiUrl, "users.seed", { email: "validation@probe.test", name: "Validation", handle: "validation" });
    const userId = object(seeded.json).id;
    expect(typeof userId).toBe("string");
    const invalid = await call(target.apiUrl, "todos.create", { userId, title: "", priority: "low" });
    expect(invalid).toHaveProperty("error");
    expect((await call(target.apiUrl, "todos.count", { userId })).json).toBe(0);
  }
});

test("an action composes a query and sends the resulting row to a webhook", async () => {
  const received: unknown[] = [];
  const server = createServer(async (request, response) => {
    const chunks: Buffer[] = [];
    for await (const chunk of request) chunks.push(Buffer.from(chunk));
    received.push(JSON.parse(Buffer.concat(chunks).toString()));
    response.writeHead(200, { "content-type": "application/json" }).end('{"received":true}');
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  try {
    const address = server.address();
    expect(address && typeof address !== "string").toBe(true);
    if (!address || typeof address === "string") throw new Error("Webhook did not bind a TCP port");
    for (const target of targets()) {
      const seeded = await call(target.apiUrl, "users.seed", { email: "webhook@probe.test", name: "Webhook", handle: "webhook" });
      const userId = object(seeded.json).id;
      const created = await call(target.apiUrl, "todos.create", { userId, title: "share me", priority: "low" });
      const id = object(created.json).id;
      expect(typeof id).toBe("string");
      expect((await call(target.apiUrl, "todos.shareToWebhook", { id, webhookUrl: `http://127.0.0.1:${address.port}` })).json)
        .toEqual({ status: 200, ok: true });
      expect(received.at(-1)).toEqual({ todo: expect.objectContaining({ id, userId, title: "share me" }) });
    }
    expect(received).toHaveLength(targets().length);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
});
