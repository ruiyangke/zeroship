import { expect } from "vitest";
import { isTypedId } from "@zeroship/server/typed-id";
import { object, rows, type Capture } from "./rpc";

export function assertDeployed(captured: Capture, run: string) {
  const result = (name: string) => {
    expect(captured, `Missing capture: ${name}`).toHaveProperty(name);
    return object(captured[name]).json;
  };
  const alice = object(result("seedA"));
  const bob = object(result("seedB"));
  expect(alice.id).toEqual(expect.any(String));
  expect(isTypedId(alice.id as string, "user")).toBe(true);
  const firstTodo = object(result("mkT1"));
  expect(firstTodo.id).toEqual(expect.any(String));
  expect(isTypedId(firstTodo.id as string, "todo")).toBe(true);
  expect(firstTodo).toMatchObject({ userId: alice.id, priority: "low" });
  expect(object(result("mkT1"))).not.toHaveProperty("userid");
  const second = object(result("mkT2"));
  expect(second).toMatchObject({ title: "walk dog", priority: "medium", done: false, tags: [], version: 1 });
  expect(captured.orphan).not.toHaveProperty("json");
  expect(captured.orphan).toMatchObject({ error: { code: "FOREIGN_KEY_VIOLATION" } });
  expect(captured.dupEmail).toMatchObject({ error: { code: "UNIQUE_VIOLATION", message: expect.any(String) } });
  expect(result("orphanN")).toBe(0);
  expect(result("countA")).toBe(5);
  expect(result("getNone")).toBeNull();
  expect(result("getT1")).toMatchObject({ id: object(result("mkT1")).id });
  expect(result("pair")).toMatchObject({ a: { id: alice.id }, b: { id: bob.id } });
  const joined = rows(result("withUser"));
  expect(joined).toHaveLength(5);
  for (const row of joined) expect(row.user).toMatchObject({ id: alice.id, email: `alice-${run}@probe.test` });
  expect(rows(result("list")).every((row) => row.userId === alice.id)).toBe(true);
  for (const name of ["p1", "p2"]) expect(result(name)).toMatchObject({ isDone: false, continueCursor: expect.stringMatching(/.+/) });
  expect(result("p3")).toMatchObject({ isDone: true });
  expect(captured.p1cur).toMatchObject({ orderBy: { id: 1 } });
  expect(result("setDone")).toMatchObject({ version: 2 });
  expect(result("archive")).toMatchObject({ version: 3 });
  expect(result("getDel")).toBeNull();
  expect(rows(result("listAfter")).some((row) => row.title === "buy milk")).toBe(false);

  const transactions = {
    txCommit: { error: null, data: { seenTitle: `c${run}-a`, inTxCount: 2, bPriority: "high" }, committedCount: 2 },
    txRoll: { error: { code: "PROBE_ROLLBACK", message: "probe rollback" }, inTxCount: 1, data: null, countAfter: 0, visibleAfter: 0 },
    txNest: { error: null, data: { innerError: { code: "PROBE_INNER" }, outerSeen: 1, innerSeen: 0 }, outerAfter: 1, innerAfter: 0 },
    txIsoNone: { countAfter: 1 },
    txIsoSer: { error: null, countAfter: 1 },
    txIsoRR: { countAfter: 1 },
    txIsoBad: { threw: null, error: { message: expect.stringContaining("unknown isolationLevel") }, countAfter: 0 },
    txD9: { deepestLevel: 9, refusedAtLevel: null, countAfter: 1 },
    txD10: { refusedAtLevel: 10, error: { code: "savepoint_depth_exceeded" }, countAfter: 0 },
    cxPar: { legs: [{ n: 1, error: null }, { n: 2, error: null }], countAfter: 2 },
    cxOvl: { aAfter: 0, b: { error: null }, bAfter: 1 },
    cxPlain: { aAfter: 0, b: { inserted: true }, bAfter: 1 },
    txBranch: { error: { code: "TRANSACTION_CONNECTION_BUSY" }, countAfter: 0 },
    txOrphan: { orphanStarted: 1, error: null, txAfter: 1, orphanAfter: 0 },
  };
  for (const [name, expected] of Object.entries(transactions)) expect.soft(result(name), name).toMatchObject(expected);
  const orphan = object(result("txOrphan"));
  expect([orphan.orphanError, orphan.orphanThrew]).toEqual(expect.arrayContaining([
    expect.objectContaining({ code: "TRANSACTION_SCOPE_EXPIRED" }),
  ]));
  expect(result("txTotal")).toBe(7);
  expect(result("cxTotal")).toBe(4);
  expect(result("bxTotal")).toBe(1);
}
