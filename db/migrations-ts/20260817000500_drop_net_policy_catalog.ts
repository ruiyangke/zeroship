import { table } from "@zeroship/migrate";

export const name = "drop_net_policy_catalog";

// The frontable-wildcard-suffix catalog is DELETED, not relocated. It existed
// to decide which wildcard grants front shared infrastructure; wildcards are no
// longer representable in the egress grammar at all (`*.example.com` does not
// parse), so there is nothing left for the catalog to answer and no config key
// replaces it. Its only writer was an operator route deleted in the same change.
//
// This comment previously said the catalog moved to a `[control]
// frontable_wildcard_suffixes` config key. No such key exists anywhere in the
// tree; that is a plan the reshape overtook.
export function up() {
  table("net_policy_catalog", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
}

export function down() {

}
