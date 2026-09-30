import { raw } from "@zeroship/migrate";

// The CDC relay's read of Control's binding rows: which database schema a
// subscriber is entitled to stream. `bound_database_schema`
// (crates/zeroship-data-cdc-server/src/source.rs) projects the database of the
// `(app, database)` pair a subscribe request named, through the live-binding
// predicate Control serves bindings from
// (`zeroship_core::live_binding::LIVE_BINDINGS_FROM_WHERE`). That statement
// reads a binding's app, database, status and both generations, and its
// database's id and status. Those columns are the whole of this grant.
//
// Column-scoped, like the relay's worker-identity read in
// 20260907000300_worker_instances.ts, because the rest of each row is not the
// relay's to hold: a binding's capability and project, a database's name,
// project and placement. The relay decides entitlement; it does not describe a
// database.
//
// The relay is a separate process under its own login, so a conjunct the
// predicate grows over another column is a read this login cannot make until a
// migration grants it. The relay's tests run the lookup as `zeroship_cdc`
// against this corpus, which is what refuses such a predicate before a
// deployment does.
export default {
  name: "cdc_relay_live_binding_grants",
  schema() {
    raw({
      sql: "GRANT SELECT (app_id, database_id, status, generation, observed_generation) ON zeroship.database_bindings TO zeroship_cdc",
      reason: "the relay admits a subscriber only through the live-binding predicate over the binding edge",
    });
    raw({
      sql: "GRANT SELECT (id, status) ON zeroship.databases TO zeroship_cdc",
      reason: "the live-binding predicate joins each binding to its database and requires that database to be active",
    });
  },
};
