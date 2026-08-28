import { table, t, now, grant } from "@zeroship/migrate";

// `zeroship.app_net_grants` is replaced, not altered. Its `(app_id, host,
// port)` shape no longer fits twice over: a rule's destination is now a DNS
// name OR an address range, and every rule now carries a verdict. Pre-launch,
// there are no rows to carry across, so the old table is dropped rather than
// backfilled.
//
// CORRECTING AN APPLIED FILE'S COMMENT, WHICH IS WHY THE NOTE IS HERE.
// `20260817000500_drop_net_policy_catalog.ts` says the frontable-wildcard-suffix
// catalog "moves to the config overlay (`[control] frontable_wildcard_suffixes`)".
// That was true when written and is false as of this file: the key is deleted
// along with the rest of the wildcard mechanism, because a wildcard destination
// is not representable in the egress grammar at all (`*.example.com` does not
// parse), so there is nothing left for the catalog to answer. That file has been
// applied to a deployed database and its source bytes are hashed in the journal,
// so it cannot be edited to say so - see the frozen-migration rule in AGENTS.md.
//
// `destination` is TEXT, not `inet`/`cidr`. The native types would need
// compio-postgres's `with-cidr-0_3` feature, which no workspace crate enables,
// and all they buy is SQL containment operators - no query here does
// containment (the registry projection reads every row and projects it). The
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
        app_id: t.uuid().notNull(),
        verdict: t.text().notNull(),
        kind: t.text().notNull(),
        destination: t.text().notNull(),
        port: t.int().notNull(),
        created_by: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        note: t.text(),
      },
      primaryKey: ["app_id", "kind", "destination", "port"],
    });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_verdict_check").add({ expr: (col) => col("verdict").in(["accept", "reject"]) });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_kind_check").add({ expr: (col) => col("kind").in(["name", "cidr"]) });
    table("app_egress_rules", { schema: "zeroship" }).check("app_egress_rules_port_check").add({ expr: (col) => col("port").ge(1).and(col("port").le(65535)) });
    table("app_egress_rules", { schema: "zeroship" }).index("app_egress_rules_app_id_idx").add({ on: ["app_id"] });
    table("app_egress_rules", { schema: "zeroship" }).foreignKey("app_egress_rules_app_id_fkey").add({ columns: ["app_id"], references: { table: "apps", columns: ["id"] }, onDelete: "cascade" });
    grant({ privileges: ["select", "insert", "update", "delete"], on: { kind: "table", schema: "zeroship", names: ["app_egress_rules"] }, to: ["zeroship_control"] });
    table("app_net_grants", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
