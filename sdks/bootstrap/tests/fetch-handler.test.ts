import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { createFetchHandler } from "../src/fetch-handler.js";

async function withDispatch<T>(fn: () => Promise<T>): Promise<T> {
  const g = globalThis as unknown as {
    __zsDispatch?: (
      rpc: Record<string, unknown>,
      name: string,
      input: unknown,
      ctx: unknown,
    ) => Promise<unknown>;
  };
  const prev = g.__zsDispatch;
  g.__zsDispatch = async (rpc, name, input, ctx) => {
    const proc = rpc[name] as ((input: unknown, ctx: unknown) => unknown) | undefined;
    if (!proc) throw Object.assign(new Error(`Method not found: ${name}`), { status: 404 });
    return proc(input, ctx);
  };
  try {
    return await fn();
  } finally {
    if (prev === undefined) delete g.__zsDispatch;
    else g.__zsDispatch = prev;
  }
}

describe("createFetchHandler — superjson wire", () => {
  test("revives rich input values before dispatch", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          inspect(input: unknown) {
            return {
              isDate: input instanceof Date,
              iso: input instanceof Date ? input.toISOString() : null,
            };
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/inspect", {
          method: "POST",
          body: JSON.stringify({
            json: "2026-01-01T00:00:00.000Z",
            meta: { values: ["Date"], v: 1 },
          }),
        }),
        {},
        {},
      );
      const body = (await res.json()) as { json: unknown };

      assert.equal(res.status, 200);
      assert.deepEqual(body.json, {
        isDate: true,
        iso: "2026-01-01T00:00:00.000Z",
      });
    });
  });

  test("sanitizes 5xx RPC errors before serializing to the client", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          fail() {
            const err = new Error("postgres://internal/schema");
            Object.assign(err, {
              status: 500,
              code: "INTERNAL",
              details: { host: "db.internal" },
            });
            throw err;
          },
        },
      }));
      const log = console.error;
      console.error = () => {};
      try {
        const res = await handler(
          new Request("https://app.test/__zeroship/v1/fail", {
            method: "POST",
            body: JSON.stringify({ json: null }),
          }),
          {},
          {},
        );
        const body = (await res.json()) as Record<string, unknown>;

        assert.equal(res.status, 500);
        assert.equal(body.message, "internal error");
        assert.equal(body.name, "Error");
        assert.equal(typeof body.request_id, "string");
        assert.equal(JSON.stringify(body).includes("postgres://internal"), false);
        assert.equal(JSON.stringify(body).includes("db.internal"), false);
      } finally {
        console.error = log;
      }
    });
  });

  // ISS-67: a `requireUser()`-shaped throw carries an explicit `status: 401`.
  // The fetch-handler's `statusFromError` must honor it (4xx), and because the
  // body-sanitizer only blanks 5xx, the "Authentication required" message and
  // the `unauthenticated` code reach the client intact — NOT masked to 500 /
  // "internal error".
  test("honors a requireUser 401 throw and does NOT mask its message", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          guarded() {
            // The exact shape the kernel/SDK requireUser throws.
            throw Object.assign(new Error("Authentication required"), {
              status: 401,
              code: "unauthenticated",
            });
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/guarded", {
          method: "POST",
          body: JSON.stringify({ json: null }),
        }),
        {},
        {},
      );
      const body = (await res.json()) as Record<string, unknown>;

      assert.equal(res.status, 401, "401 throw must surface as 401, not 500");
      assert.equal(body.message, "Authentication required", "4xx message must NOT be masked");
      assert.notEqual(body.message, "internal error");
      assert.equal(body.code, "unauthenticated");
    });
  });

  // C1 — the fetch fall-through to `default.fetch` MUST be gated on schema
  // readiness, symmetric with the RPC dispatcher. Otherwise a `default.fetch`
  // handler doing `env.db.users.insert(...)` runs CONCURRENTLY with the cold-
  // boot SQLite migration (the destructive 12-step rebuild) and observes a
  // half-built schema ("no such table"). We model the migration as a deferred
  // promise that flips `schemaMigrated` to true only when it resolves; the user
  // fetch reads that flag. The gate must make the handler observe `true`.
  describe("C1 — schema-readiness gate on default.fetch", () => {
    test("awaits schema readiness before invoking the user fetch handler", async () => {
      // The "migration in flight" — resolves on the next macrotask, flipping the
      // flag. A handler that runs BEFORE this resolves would read `false`.
      let schemaMigrated = false;
      let resolveMigration!: () => void;
      const migration = new Promise<void>((res) => {
        resolveMigration = () => {
          schemaMigrated = true;
          res();
        };
      });
      // Kick the migration onto a later turn so an UNGATED fetch would win the race.
      setTimeout(() => resolveMigration(), 0);

      const handler = createFetchHandler(
        async () => ({
          userDefault: {},
          fetch(_req: Request) {
            // This stands in for `env.db.<coll>.find()` inside default.fetch:
            // it must only run once the migration has applied.
            return new Response(JSON.stringify({ migrated: schemaMigrated }), {
              status: schemaMigrated ? 200 : 503,
            });
          },
          rpc: {},
        }),
        // The gate the dev-entry / runtime wires in: await the in-flight migration.
        () => migration,
      );

      const res = await handler(new Request("https://app.test/"), {}, {});
      const body = (await res.json()) as { migrated: boolean };

      assert.equal(res.status, 200, "gated fetch must run AFTER the migration applied");
      assert.equal(
        body.migrated,
        true,
        "C1: default.fetch must observe the fully-migrated schema, never race the cold-boot migration",
      );
    });

    test("RED-control: WITHOUT the gate the same handler races the migration", async () => {
      // Proves the test is meaningful: drop the `awaitSchemaReady` arg and the
      // identical handler observes the un-migrated state (the pre-fix bug).
      let schemaMigrated = false;
      setTimeout(() => {
        schemaMigrated = true;
      }, 0);

      const ungated = createFetchHandler(async () => ({
        userDefault: {},
        fetch(_req: Request) {
          return new Response(JSON.stringify({ migrated: schemaMigrated }), {
            status: schemaMigrated ? 200 : 503,
          });
        },
        rpc: {},
      }));

      const res = await ungated(new Request("https://app.test/"), {}, {});
      const body = (await res.json()) as { migrated: boolean };
      assert.equal(
        body.migrated,
        false,
        "control: the ungated handler races the migration (this is the bug the gate fixes)",
      );
    });

    test("surfaces a rejected schema chain as a sanitized 500, not a half-built read", async () => {
      // A failed cold-boot migration must make the fetch fail LOUD (clear error),
      // never hang and never run the handler against a broken DB.
      const handler = createFetchHandler(
        async () => ({
          userDefault: {},
          fetch(_req: Request) {
            // Must NOT be reached — the gate rejects before we get here.
            return new Response("should not run", { status: 200 });
          },
          rpc: {},
        }),
        () => Promise.reject(new Error("registerModel: rebuild failed: disk I/O error")),
      );

      const log = console.error;
      console.error = () => {};
      try {
        const res = await handler(new Request("https://app.test/"), {}, {});
        const body = (await res.json()) as Record<string, unknown>;
        assert.equal(res.status, 500, "a failed schema-apply must surface as 500");
        assert.equal(
          JSON.stringify(body).includes("disk I/O error"),
          false,
          "the raw migration error must be sanitized out of the 5xx body",
        );
      } finally {
        console.error = log;
      }
    });
  });

  test("serializes rich output values with meta", async () => {
    await withDispatch(async () => {
      const handler = createFetchHandler(async () => ({
        userDefault: {},
        fetch: undefined,
        rpc: {
          today() {
            return new Date("2026-01-01T00:00:00.000Z");
          },
        },
      }));

      const res = await handler(
        new Request("https://app.test/__zeroship/v1/today", {
          method: "POST",
          body: JSON.stringify({ json: null }),
        }),
        {},
        {},
      );
      const text = await res.text();

      assert.equal(res.status, 200);
      assert.match(text, /"json":"2026-01-01T00:00:00\.000Z"/);
      assert.match(text, /"meta":\{"values":\["Date"\],"v":1\}/);
    });
  });
});
