// op.* migration fixture — the FULL §3.2 FLUENT DDL surface (PR3). Authored in
// the headline form (the chainable `t.*` column lexicon + the `{ col: t.* }`
// createTable map + the `(b) => …` scoped-builder overload + the typed
// `(table, spec)` constraint/index adders + `addColumn(table, name, t.*)` +
// `alterColumn`/`renameColumn`/`batchAlterTable`). Proves the fluent authoring
// style records the IDENTICAL frozen wire ops as the legacy positional style.
//
// Covers EVERY t.* column type + EVERY modifier:
//   t.id() (uuid PK + genRandomUuid default), t.text().notNull(), t.numeric(),
//   t.timestamp().default({fn:"now"}), t.uuid(), t.bytes(), t.boolean().default,
//   t.json(), t.ref(target), t.vector(n), t.geoPoint(), t.string(), t.int(),
//   t.bigInt(), t.float(), t.encrypted({of}), and .unique().
import {
  createTable,
  addColumn,
  alterColumn,
  renameColumn,
  addForeignKey,
  addUnique,
  addCheck,
  dropConstraint,
  createIndex,
  batchAlterTable,
  t,
} from "@zeroship/migrate";

export default {
  name: "fluent_ddl",

  up() {
    // The fluent createTable map — every t.* type + modifier exercised.
    createTable("accounts", {
      id: t.id(), // uuid PK, default gen_random_uuid()
      email: t.text().notNull().unique(),
      balance: t.numeric(12, 2).notNull().default({ decimal: "0.00" }),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      external_id: t.uuid(),
      avatar: t.bytes(),
      active: t.boolean().notNull().default(true),
      profile: t.json(),
      owner: t.ref("users"),
      embedding: t.vector(1536),
      location: t.geoPoint(),
      label: t.string(),
      hits: t.int().notNull().default(0),
      big_hits: t.bigInt(),
      ratio: t.float(),
      secret: t.encrypted({ of: t.text() }),
    });

    // The (b) => … scoped-builder overload — table-scoped constraints/indexes.
    createTable(
      "memberships",
      { account_id: t.uuid().notNull(), team: t.text().notNull() },
      (b) => {
        b.primaryKey(["account_id", "team"]);
        b.unique(["team"], { name: "memberships_team_uq" });
        b.index(["account_id"], { name: "memberships_account_idx" });
        b.check((c) => c("team").isNotNull(), { name: "memberships_team_chk" });
        b.foreignKey({
          columns: ["account_id"],
          references: { table: "accounts", columns: ["id"] },
          name: "memberships_account_fk",
        });
      },
    );

    // addColumn with a fluent ColumnDef (its modifiers ride along).
    addColumn("accounts", "status", t.text().notNull().default("new"));

    // The typed (table, spec) constraint adders — `name` lives in the spec, the
    // references are named fields (NOT transposable positionals).
    addForeignKey("memberships", {
      columns: ["team"],
      references: { table: "teams", columns: ["name"] },
      name: "memberships_team_fk",
    });
    addUnique("accounts", { columns: ["external_id"], name: "accounts_external_uq" });
    addCheck("accounts", { expr: (c) => c("balance").ge(0), name: "accounts_balance_chk" });
    dropConstraint("accounts", { name: "accounts_legacy_chk", type: "check" });

    // The fluent alterColumn change descriptor (type + nullability forms).
    alterColumn("accounts", "balance", { type: t.numeric(14, 2) });
    alterColumn("accounts", "profile", { nullable: false });

    // renameColumn carries the post-rename ColumnDef.
    renameColumn("accounts", "label", "display_label", t.text());

    // A partial unique index authored via the (table, spec) form with a fluent
    // `where` predicate.
    createIndex("accounts", {
      columns: ["email"],
      name: "accounts_active_email_idx",
      unique: true,
      where: (c) => c("active").isTrue(),
    });

    // batchAlterTable — the SQLite-safe rebuild scoped builder.
    batchAlterTable("accounts", (b) => {
      b.addColumn("nickname", t.text());
      b.alterColumn("nickname", { nullable: false });
    });
  },
};
