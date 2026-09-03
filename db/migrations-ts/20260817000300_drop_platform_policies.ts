import { table } from "@zeroship/migrate";

// The table backed three operator routes that wrote it and nothing that read
// it. Enforcement takes the boot-time `load_platform_policies()` value - the
// static `.cedar` set compiled into the binary - and never swaps it, so a row
// here changed no authorization decision anywhere. What the routes preserved
// was the BELIEF in a runtime override lever, which is worse than not having
// one: an incident responder would reach for it and it would silently no-op.
//
// The three routes go in the same change, so nothing is left writing rows that
// nothing reads.
export default {
  name: "drop_platform_policies",
  schema() {
    table("platform_policies", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
