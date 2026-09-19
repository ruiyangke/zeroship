import { table, t, now, grant } from "@zeroship/migrate";

// `destination` is TEXT, not `inet`/`cidr`. A destination is a DNS NAME or a
// range, and no address type holds a name; all the native types would add is
// SQL containment operators, and no query here does containment (the registry
// projection reads every row and projects it). The
// cost is that the database will accept a spelling only Rust rejects, and that
// duplicate spellings of one range are distinct primary keys. Both are paid at
// the authoring boundary: the control plane is the sole writer and it
// canonicalises the name and the CIDR before either reaches this table.
//
// `verdict` is deliberately NOT part of the primary key. One
// `(kind, destination, port)` carries at most one verdict, so flipping a rule
// from accept to reject is an UPDATE rather than an INSERT that silently
// coexists with its opposite. The evaluator still defines the contradictory
// case (reject wins) because it must be total; the schema keeps the common
// case from producing one.
//
// There is no ordering column. Verdicts compose as deny-overrides on an
// unordered set, so position has no meaning and the registry projection's
// existing ORDER BY stays a determinism device rather than becoming
// security-relevant.
export default {
  name: "app_egress_rules",
  schema() {
    table("app_egress_rules", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        app_id: t.text().notNull(),
        verdict: t.text().notNull(),
        kind: t.text().notNull(),
        destination: t.text().notNull(),
        port: t.int().notNull(),
        created_by: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        note: t.text(),
      },
      primaryKey: ["id"],
    });
    table("app_egress_rules", { schema: "zeroship" }).unique("app_egress_rules_natural_key").add({ columns: ["app_id", "kind", "destination", "port"] });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_verdict_check").add({ expr: (col) => col("verdict").in(["accept", "reject"]) });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_kind_check").add({ expr: (col) => col("kind").in(["name", "cidr"]) });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_port_check").add({ expr: (col) => col("port").ge(1).and(col("port").le(65535)) });
    table("app_egress_rules", { schema: "zeroship" })
      .check("app_egress_rules_created_by_usr_shape")
      .add({ expr: (col) => col("created_by").regex("^usr_[0-9a-z]{25}$") });
    table("app_egress_rules", { schema: "zeroship" }).index("app_egress_rules_app_id_idx").add({ on: ["app_id"] });
    table("app_egress_rules", { schema: "zeroship" }).foreignKey("app_egress_rules_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_egress_rules"] }, to: ["zeroship_control"] });
  },
};
