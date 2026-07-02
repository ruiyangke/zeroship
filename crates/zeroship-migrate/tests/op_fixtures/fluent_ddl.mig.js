// op.* migration fixture — the FULL FLUENT DDL surface, authored via the SOLE
// public `table()` entry (design 2026-06-25-op-dsl-fluent-redesign.md). Covers the
// create() all-object form (columns + table-level unique/check/fk/index), the
// .column()/.foreignKey()/.unique()/.check()/.constraint()/.index() selectors, and
// the immutable t.* lexicon. Proves the fluent authoring records the frozen wire
// ops (the byte-identity oracle's golden).
//
// Covers EVERY t.* column type + EVERY modifier:
//   t.id() (uuid PK + genRandomUuid default), t.text().notNull(), t.numeric(),
//   t.timestamp().default({fn:"now"}), t.uuid(), t.bytes(), t.boolean().default,
//   t.json(), t.ref(target), t.vector(n), t.geoPoint(), t.text() (was t.string —
//   alias removed), t.integer() (was t.int), t.bigInt(), t.float(),
//   t.encrypted({of}), and .unique().
import { table, t } from "@zeroship/migrate";

export default {
  name: "fluent_ddl",

  up() {
    table("accounts").create({
      columns: {
        id: t.id(), // uuid PK, default gen_random_uuid()
        email: t.text().notNull().unique(),
        balance: t.numeric(12, 2).notNull().default({ decimal: "0.00" }),
        authored_at: t.timestamp().notNull().default({ fn: "now" }),
        external_id: t.uuid(),
        avatar: t.bytes(),
        active: t.boolean().notNull().default(true),
        profile: t.json(),
        owner: t.ref("users"),
        embedding: t.vector(1536),
        location: t.geoPoint(),
        label: t.text(), // was t.string() — the alias is removed (§7)
        hits: t.integer().notNull().default(0),
        big_hits: t.bigInt(),
        ratio: t.float(),
        secret: t.encrypted({ of: t.text() }),
      },
    });

    table("memberships").create({
      columns: { account_id: t.uuid().notNull(), team: t.text().notNull() },
      uniques: [{ name: "memberships_team_uq", columns: ["team"] }],
      checks: [{ name: "memberships_team_chk", expr: (c) => c("team").isNotNull() }],
      foreignKeys: [
        {
          name: "memberships_account_fk",
          columns: ["account_id"],
          references: { table: "accounts", columns: ["id"] },
        },
      ],
      indexes: [{ name: "memberships_account_idx", columns: ["account_id"] }],
    });

    table("accounts").column("status").add({ type: t.text().notNull().default("new") });

    table("memberships").foreignKey("memberships_team_fk").add({
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
    });
    table("accounts").unique("accounts_external_uq").add({ columns: ["external_id"] });
    table("accounts").check("accounts_balance_chk").add({ expr: (c) => c("balance").ge(0) });
    table("accounts").constraint("accounts_legacy_chk").drop();

    table("accounts").column("balance").setType({ to: t.numeric(14, 2) });
    table("accounts").column("profile").setNotNull();

    table("accounts").column("label").rename({ to: "display_label", type: t.text() });

    table("accounts").index("accounts_active_email_idx").add({
      columns: ["email"],
      unique: true,
      where: (c) => c("active").isTrue(),
    });

    table("accounts").column("nickname").add({ type: t.text() });
    table("accounts").column("nickname").setNotNull();
  },
};
