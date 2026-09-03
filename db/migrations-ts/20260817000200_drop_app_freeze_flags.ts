import { table } from "@zeroship/migrate";

// Both columns were operator freeze levers, written only by the two admin
// routes deleted in the same change and read only by the Cedar entity builder.
// Neither ever reached the data plane: an app carrying either flag kept serving
// traffic and kept writing its own database, storage and KV, so what they froze
// was the creator's ability to change the app, not the app.
//
// They drop together because they were one lever with two names:
// `suspended_apps.cedar` and `audit_locked.cedar` forbade the same eight
// actions, in the same order, differing only in the attribute tested. With the
// routes gone nothing can set either column, and a forbid rule reading a field
// no code writes can never fire.
//
// One migration, not two: they are the same removal, they were introduced
// together on `zeroship.apps`, and splitting them would leave an intermediate
// state whose only difference is which of two identical dead levers survives.
export default {
  name: "drop_app_freeze_flags",
  schema() {
    table("apps", { schema: "zeroship" }).column("suspended").drop({ ifExists: true });
    table("apps", { schema: "zeroship" }).column("audit_locked").drop({ ifExists: true });
  },
};
