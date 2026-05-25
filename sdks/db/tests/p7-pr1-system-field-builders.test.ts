/**
 * **P7 PR 1** — Schema DSL + reserved-name validator + Cargo for
 * platform system fields.
 *
 * Coverage:
 * - `t.id(prefix?)` — emits `{ type: "id", idPrefix? }`.
 * - `t.timestamp().auto_now()` — emits `{ type: "date", timestampAuto: "now" }`.
 * - `t.timestamp().auto_now_on_update()` — emits
 *   `{ type: "date", timestampAuto: "now_on_update" }`.
 * - `t.actor()` — emits `{ type: "actor", actorNullable: true }`.
 * - `normalizeSchema` refuses creator-declared system-field names with
 *   `reserved_system_field_name` (mirrors the Rust-side reservation).
 * - Type-level: `Row<S>` automatically includes the seven system fields.
 *
 * No CREATE TABLE / CRUD wiring is exercised — those land in PR 2/3+.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, type Row } from "@zeroship/db";
import { TypeBuilder } from "@zeroship/db/internal";
import { normalizeSchema } from "@zeroship/bootstrap/install-schema";

describe("P7 PR 1 — t.id() builder", () => {
  test("t.id() emits the `id` discriminator without a prefix", () => {
    const tb = t.id();
    assert.ok(tb instanceof TypeBuilder);
    const def = tb.toFieldDef();
    assert.equal(def.type, "id");
    assert.equal(def.idPrefix, undefined);
  });

  test("t.id(prefix) carries the prefix on the wire shape", () => {
    const def = t.id("post").toFieldDef();
    assert.equal(def.type, "id");
    assert.equal(def.idPrefix, "post");
  });

  test("t.id() refuses an empty prefix", () => {
    assert.throws(
      () => t.id(""),
      (e: unknown) =>
        (e as { code?: string })?.code === "id_invalid_prefix",
    );
  });

  test("t.id() refuses a prefix with non-allowlist chars", () => {
    assert.throws(
      () => t.id("Post-Type"),
      (e: unknown) =>
        (e as { code?: string })?.code === "id_invalid_prefix",
    );
  });
});

describe("P7 PR 1 — t.timestamp() auto-population modifiers", () => {
  test("bare t.timestamp() does NOT set timestampAuto", () => {
    const def = t.timestamp().toFieldDef();
    assert.equal(def.type, "date");
    assert.equal(def.timestampAuto, undefined);
  });

  test("t.timestamp().auto_now() emits timestampAuto = 'now'", () => {
    const def = t.timestamp().auto_now().toFieldDef();
    assert.equal(def.type, "date");
    assert.equal(def.timestampAuto, "now");
  });

  test("t.timestamp().auto_now_on_update() emits timestampAuto = 'now_on_update'", () => {
    const def = t.timestamp().auto_now_on_update().toFieldDef();
    assert.equal(def.type, "date");
    assert.equal(def.timestampAuto, "now_on_update");
  });

  test(".auto_now() on a non-timestamp builder throws auto_now_on_non_timestamp", () => {
    assert.throws(
      () => t.string().auto_now(),
      (e: unknown) =>
        (e as { code?: string })?.code === "auto_now_on_non_timestamp",
    );
  });

  test(".auto_now_on_update() on a non-timestamp builder throws auto_now_on_non_timestamp", () => {
    assert.throws(
      () => t.number().auto_now_on_update(),
      (e: unknown) =>
        (e as { code?: string })?.code === "auto_now_on_non_timestamp",
    );
  });
});

describe("P7 PR 1 — t.actor() builder", () => {
  test("t.actor() emits the `actor` discriminator, nullable by default", () => {
    const tb = t.actor();
    assert.ok(tb instanceof TypeBuilder);
    const def = tb.toFieldDef();
    assert.equal(def.type, "actor");
    assert.equal(def.actorNullable, true);
  });

  test("t.actor().nullable() stays nullable (no-op chain)", () => {
    const def = t.actor().nullable().toFieldDef();
    assert.equal(def.type, "actor");
    assert.equal(def.actorNullable, true);
  });
});

describe("P7 PR 1 — reserved-name validator (SDK side)", () => {
  // The seven names must be refused at schema-declaration time.
  // Mirrors the Rust-side `SYSTEM_FIELD_NAMES` in
  // `crates/plugin-db/src/query.rs`.
  const SYSTEM_FIELDS = [
    "id",
    "created_at",
    "updated_at",
    "created_by",
    "updated_by",
    "version",
    "deleted_at",
  ];

  for (const name of SYSTEM_FIELDS) {
    test(`schema declaration with "${name}" throws reserved_system_field_name`, () => {
      assert.throws(
        () => normalizeSchema({ [name]: t.string() }),
        (e: unknown) => {
          const code = (e as { code?: string })?.code;
          const msg = (e as { message?: string })?.message ?? "";
          return (
            code === "reserved_system_field_name" &&
            msg.includes(name) &&
            msg.includes("reserved")
          );
        },
      );
    });
  }

  test("reservation error message lists all seven system fields in the hint", () => {
    try {
      normalizeSchema({ id: t.string() });
      assert.fail("normalizeSchema must throw on a reserved name");
    } catch (e: unknown) {
      const msg = (e as { message?: string })?.message ?? "";
      for (const name of SYSTEM_FIELDS) {
        assert.ok(
          msg.includes(name),
          `error message must list system field ${name}; got: ${msg}`,
        );
      }
    }
  });

  test("non-reserved field names normalise cleanly", () => {
    const norm = normalizeSchema({
      title: t.string(),
      body: t.string(),
      author_id: t.string(),
    });
    assert.equal(norm.title.type, "string");
    assert.equal(norm.body.type, "string");
    assert.equal(norm.author_id.type, "string");
  });
});

describe("P7 PR 1 — Row<S> auto-includes the seven system fields", () => {
  test("Row<S> type carries every system field at compile time", () => {
    // Type-level assertion via assignability — if `Row<S>` were missing
    // a system field, the literal below would not be assignable.
    type UserSchema = {
      title: TypeBuilder<string, true>;
    };
    // The `Row<UserSchema>` must include every system field shape.
    // Build a literal that names every system field; assignment to
    // `Row<UserSchema>` succeeds iff the type carries them all.
    // PR 1 keeps legacy number-shaped id/timestamps; PR 3 widens to
    // typed_id strings + ISO 8601 timestamps. The system-field
    // PRESENCE is what PR 1 pins; the wire shapes evolve later.
    const row: Row<UserSchema> = {
      title: "hello",
      id: 42,
      created_at: 1700000000000,
      updated_at: 1700000000000,
      created_by: null,
      updated_by: null,
      version: 1,
      deleted_at: null,
      // Legacy camelCase aliases retained during the P7 migration
      // window so existing callers (`db.users.find({ created_at })`)
      // type-check unchanged.
      created_at: 1700000000000,
      updated_at: 1700000000000,
    };
    assert.equal(row.title, "hello");
    assert.equal(row.id, 42);
    assert.equal(row.version, 1);
    assert.equal(row.created_by, null);
    assert.equal(row.deleted_at, null);
  });

  test("Row<S> permits nullable created_by / updated_by / deleted_at", () => {
    type UserSchema = { title: TypeBuilder<string, true> };
    const liveRow: Row<UserSchema> = {
      title: "hi",
      id: 1,
      created_at: 1700000000000,
      updated_at: 1700000000000,
      created_by: "usr_abc",
      updated_by: "usr_abc",
      version: 3,
      deleted_at: null,
      created_at: 1700000000000,
      updated_at: 1700000000000,
    };
    const deletedRow: Row<UserSchema> = {
      title: "hi",
      id: 2,
      created_at: 1700000000000,
      updated_at: 1700000000000,
      created_by: null,
      updated_by: null,
      version: 5,
      deleted_at: 1700000060000,
      created_at: 1700000000000,
      updated_at: 1700000000000,
    };
    assert.equal(liveRow.deleted_at, null);
    assert.equal(typeof deletedRow.deleted_at, "number");
  });
});
