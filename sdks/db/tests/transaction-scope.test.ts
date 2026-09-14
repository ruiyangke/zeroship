import { describe, test } from "node:test";
import assert from "node:assert/strict";

import { t } from "../src/index.js";
import { env } from "zeroship";
import type { TxCollection } from "../src/db-types.js";
import type { NativeDb } from "../src/native.js";
import { installSchemaForTest } from "./_install-helper.js";

type NativeCall = {
  filter: Record<string, unknown>;
  opts: Record<string, unknown>;
};

function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve = () => {};
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
}

function fixture() {
  const calls: NativeCall[] = [];
  const native = {
    async transaction(callback: (raw: unknown) => unknown) {
      return callback(undefined);
    },
    collection() {
      let finishSubscription: ((event: null) => void) | undefined;
      return {
        async find(filter: Record<string, unknown>, opts: Record<string, unknown>) {
          calls.push({ filter, opts });
          return [{ id: "doc_1", secret_col: "plain" }];
        },
        async insert(row: Record<string, unknown>) {
          calls.push({ filter: row, opts: {} });
          return { id: "doc_1", ...row };
        },
        async read() {
          calls.push({ filter: {}, opts: {} });
          return [];
        },
        openSubscription() {
          return {
            async ready() {},
            next() {
              return new Promise<null>((resolve) => {
                finishSubscription = resolve;
              });
            },
            close() {
              finishSubscription?.(null);
            },
          };
        },
      };
    },
  } as unknown as NativeDb;
  (env as { db?: unknown }).db = native;
  const db = installSchemaForTest(
    { docs: { secret: t.string().required() } },
    {
      native,
      naming: {
        toColumn: (field) => field === "secret" ? "secret_col" : field,
        toField: (column) => column === "secret_col" ? "secret" : column,
      },
    },
  );
  return { calls, db };
}

describe("transaction scope", () => {
  test("expired collections, queries, and aliases reject before native dispatch", async () => {
    const { calls, db } = fixture();
    let escapedCollection: TxCollection | undefined;
    let escapedQuery: PromiseLike<unknown[]> | undefined;
    let escapedAlias: ReturnType<TxCollection["as"]> | undefined;
    let escapedRead: { all(): Promise<unknown> } | undefined;

    const result = await db.transaction(async (tx) => {
      escapedCollection = tx.docs as unknown as TxCollection;
      escapedQuery = tx.docs.find({}) as PromiseLike<unknown[]>;
      const alias = tx.docs.as("d");
      escapedAlias = alias as unknown as ReturnType<TxCollection["as"]>;
      escapedRead = tx.from(alias).select({ doc: alias.row() });
      return null;
    });
    assert.equal(result.error, null);
    const before = calls.length;

    await assert.rejects(escapedCollection!.get("doc_1"), {
      code: "TRANSACTION_SCOPE_EXPIRED",
    });
    await assert.rejects(escapedCollection!.insert({ secret: "late" }), {
      code: "TRANSACTION_SCOPE_EXPIRED",
    });
    await assert.rejects(Promise.resolve(escapedQuery!), {
      code: "TRANSACTION_SCOPE_EXPIRED",
    });
    await assert.rejects(escapedRead!.all(), {
      code: "TRANSACTION_SCOPE_EXPIRED",
    });
    const readFromAlias = db.from(escapedAlias!).select({ doc: escapedAlias!.row() });
    const aliasResult = await readFromAlias.all();
    assert.equal(aliasResult.data, null);
    assert.equal((aliasResult.error as Error & { code?: string } | null)?.code, "TRANSACTION_SCOPE_EXPIRED");
    assert.equal(calls.length, before);
  });

  test("a pending transaction does not change reads in a sibling continuation", async () => {
    const { calls, db } = fixture();
    const entered = deferred();
    const release = deferred();

    const transaction = db.transaction(async () => {
      entered.resolve();
      await release.promise;
      return null;
    });
    await entered.promise;

    const outside = await db.docs.get("doc_1");
    assert.equal(outside.error, null);
    assert.deepEqual(calls.at(-1)?.filter, { id: { $in: ["doc_1"] } });
    const live = db.live(async () => [], { tables: ["docs"] });
    assert.deepEqual(await live.next(), { value: [], done: false });
    live.close();

    release.resolve();
    assert.equal((await transaction).error, null);
  });

  test("transaction get and find forward unmask audit context", async () => {
    const { calls, db } = fixture();
    const actor = { role: "support", id: "usr_1" };

    const result = await db.transaction(async (tx) => {
      await tx.docs.get("doc_1", {
        actor,
        unmask: ["secret"],
        unmaskReason: "support request",
      });
      await tx.docs.find({}, {
        actor,
        unmask: ["secret"],
        unmaskReason: "support request",
      });
      return null;
    });
    assert.equal(result.error, null);
    assert.deepEqual(calls.map(({ opts }) => opts), [
      {
        limit: 1,
        actor,
        unmask: ["secret_col"],
        unmaskReason: "support request",
      },
      {
        actor,
        unmask: ["secret_col"],
        unmaskReason: "support request",
      },
    ]);
  });
});
