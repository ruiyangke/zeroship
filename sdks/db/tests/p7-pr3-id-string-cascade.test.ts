/**
 * **P7 PR 3** — SDK-side cascade of the `id: string` (typed_id) shape.
 *
 * Covers:
 * - `IdLoader<R extends { id: string }>` accepts typed_id keys.
 * - `Collection._loadById(id: string)` routes through the loader.
 * - The `Row<S>['id']` type is `string` at compile time.
 * - `insert()` round-trips the platform-minted typed_id back to the caller.
 * - `insert()` without an `id` lets the platform mint one (no client-side
 *   mint required).
 *
 * Mirrors the Rust-side `dispatch_insert` auto-mint pass landed in this
 * PR (see `crates/plugin-db/src/crud/system_fields_pass.rs`).
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { IdLoader } from "../src/loader.js";
import { model } from "@zeroship/bootstrap/install-schema";
import { t, type Row } from "@zeroship/db";
import type { TypeBuilder } from "../src/internal.js";

type AnyRec = Record<string, unknown>;

describe("P7 PR 3 — IdLoader accepts typed_id strings", () => {
  test("id_loader_accepts_typed_id_string", async () => {
    const loader = new IdLoader<{ id: string; name: string }>(
      async (ids) => {
        const m = new Map<string, { id: string; name: string }>();
        for (const i of ids) m.set(i, { id: i, name: `name-${i}` });
        return m;
      },
    );
    const row = await loader.load("usr_01HXY3Z9PQR2STUV4WXY5Z6789");
    assert.equal(row?.id, "usr_01HXY3Z9PQR2STUV4WXY5Z6789");
    assert.equal(row?.name, "name-usr_01HXY3Z9PQR2STUV4WXY5Z6789");
  });

  test("id_loader_dedupes_string_ids_in_a_microtask", async () => {
    let flushCount = 0;
    let flushedIds: readonly string[] | null = null;
    const loader = new IdLoader<{ id: string; v: number }>(
      async (ids) => {
        flushCount += 1;
        flushedIds = ids;
        const m = new Map<string, { id: string; v: number }>();
        for (const i of ids) m.set(i, { id: i, v: i.length });
        return m;
      },
    );
    const results = await Promise.all([
      loader.load("post_x"),
      loader.load("post_x"),
      loader.load("post_y"),
    ]);
    assert.equal(flushCount, 1, "one batch per microtask");
    // Distinct ids deduped — order is insertion-order.
    assert.deepEqual([...(flushedIds ?? [])], ["post_x", "post_y"]);
    for (const r of results) assert.ok(r);
  });
});

describe("P7 PR 3 — Collection.get(string) routes through the loader", () => {
  function makeNative(rows: Record<string, AnyRec>) {
    const calls: { find: AnyRec[]; findOne: AnyRec[] } = { find: [], findOne: [] };
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async findOne(filter: AnyRec) {
            calls.findOne.push(filter);
            const id = filter.id;
            return typeof id === "string" ? rows[id] ?? null : null;
          },
          async find(filter: AnyRec) {
            calls.find.push(filter);
            const idClause = filter.id as { $in?: string[] } | undefined;
            if (idClause && Array.isArray(idClause.$in)) {
              return idClause.$in.map((i) => rows[i]).filter(Boolean);
            }
            return [];
          },
        };
      },
    };
    return { native: native as unknown as ZeroshipDb, calls };
  }

  test("collection_load_by_id_string", async () => {
    const { native, calls } = makeNative({
      post_01: { id: "post_01", title: "hello" },
      post_02: { id: "post_02", title: "world" },
    });
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
    );
    const [a, b] = await Promise.all([
      Posts.get("post_01"),
      Posts.get("post_02"),
    ]);
    assert.equal(a.error, null);
    assert.equal(b.error, null);
    assert.equal((a.data as AnyRec)?.title, "hello");
    assert.equal((b.data as AnyRec)?.title, "world");

    // Both concurrent gets coalesce into one batched find.
    assert.equal(calls.find.length, 1);
    const idClause = calls.find[0].id as { $in: string[] };
    assert.deepEqual(
      [...idClause.$in].sort(),
      ["post_01", "post_02"],
      "loader sends typed_id strings on the wire",
    );
  });

});

describe("P7 PR 3 — Row<S>['id'] type widened to string", () => {
  test("row_id_type_is_string", () => {
    type UserSchema = { name: TypeBuilder<string, true> };
    // Compile-time check via assignability — the literal succeeds iff
    // `Row<UserSchema>['id']` accepts a string. A pre-PR 3 build of the
    // SDK would refuse this assignment (id was `number`).
    const row: Row<UserSchema> = {
      name: "alice",
      id: "usr_01HXY3Z9PQR2STUV4WXY5Z6789",
      created_at: 1700000000000,
      updated_at: 1700000000000,
      created_by: "usr_actor",
      updated_by: "usr_actor",
      version: 1,
      deleted_at: null,
    };
    // Runtime sanity — value preserved.
    assert.equal(typeof row.id, "string");
    assert.equal(row.id, "usr_01HXY3Z9PQR2STUV4WXY5Z6789");
    assert.equal(row.name, "alice");
  });
});

describe("P7 PR 3 — insert returns the platform-minted id", () => {
  test("insert_returns_row_with_string_id", async () => {
    // The native `insert` mock simulates the Rust-side
    // `dispatch_insert` returning the row with a minted typed_id.
    // The SDK passes the user doc through; the Rust side mints `id`
    // and the `RETURNING *` row carries it back.
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async insert(doc: AnyRec) {
            // Auto-mint here to mimic the Rust-side behaviour.
            const id = doc.id ?? "post_AUTO123";
            return { ...doc, id };
          },
        };
      },
    } as unknown as ZeroshipDb;
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
    );

    const { data, error } = await Posts.insert({ title: "hello" });
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(typeof data!.id, "string", "Row.id is a string");
    assert.equal(data!.id, "post_AUTO123");
    assert.equal(data!.title, "hello");
  });

  test("insert_without_id_lets_platform_mint", async () => {
    let receivedDoc: AnyRec | null = null;
    const native = {
      registerModel: () => Promise.resolve(),
      collection() {
        return {
          async insert(doc: AnyRec) {
            receivedDoc = doc;
            // The Rust side would mint id here; the test asserts the SDK
            // does NOT mint on its own (no `id` in the outbound doc).
            return { ...doc, id: "post_MINTED_BY_RUST" };
          },
        };
      },
    } as unknown as ZeroshipDb;
    const Posts = model(
      "posts",
      { title: t.string().required() },
      native,
    );
    const { data, error } = await Posts.insert({ title: "no id supplied" });
    assert.equal(error, null);
    assert.ok(data);
    assert.equal(
      (receivedDoc as AnyRec).id,
      undefined,
      "SDK must NOT mint id client-side — Rust does it (see system_fields_pass)",
    );
    assert.equal(data!.id, "post_MINTED_BY_RUST");
  });
});
