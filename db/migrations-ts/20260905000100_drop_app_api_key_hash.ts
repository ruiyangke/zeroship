import { table } from "@zeroship/migrate";

// A plaintext secret column stored beside its own hash, gating nothing.
//
// `apps.api_key_hash` existed for exactly one reader: the gateway's
// `check_api_key`, which compared an `X-Api-Key` header against it. RPC v1
// replaced that with the compiled per-resource `EffectivePolicy`
// (`auth: anonymous|user`) and deleted the only call site; the check, the
// `RouteEntry.api_key_hash` that carried the value to the edge, and the column
// itself outlived it uncalled. All three go together, so nothing is left
// holding a credential digest no code can present a credential to.
//
// `apps.api_key` - the PLAINTEXT half - IS STILL HERE, and that is not an
// oversight. `AppRecord.api_key` still has production readers, so removing the
// column is a separate change with its own consumers to re-plumb. What remains
// is inert: no code hashes it, compares it, or refuses a request because of it.
// Until it goes, treat an `X-Api-Key` header anywhere in this repository's
// harnesses as decoration.
export default {
  name: "drop_app_api_key_hash",
  schema() {
    table("apps", { schema: "zeroship" }).column("api_key_hash").drop({ ifExists: true });
  },
};
