import { table } from "@zeroship/migrate";

// The frontable-wildcard-suffix catalog moves to the config overlay
// (`[control] frontable_wildcard_suffixes`), read at boot by the two paths that
// consulted this table: the creator net-grant writer and the registry
// projection the worker revalidates against.
//
// It is deployment-wide policy, not per-tenant state, so a file is where it
// belongs; its only writer was an operator route being deleted. The
// fail-closed contract is unchanged and is the reason this is not a
// housekeeping move: absent config means the catalog is UNAVAILABLE and every
// wildcard grant is refused, never permitted.
export default {
  name: "drop_net_policy_catalog",
  schema() {
    table("net_policy_catalog", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
