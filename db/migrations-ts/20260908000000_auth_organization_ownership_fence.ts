import { grant, raw } from "@zeroship/migrate";

// The account reaper decides the ownership rule INSIDE its erasure transaction,
// under the same `zeroship.organizations` row lock every membership mutation in
// the control plane takes (`crates/zeroship-auth/src/cron/account_reaper.rs`,
// `refuse_if_it_strands_an_organization`). Without these grants that statement
// raises 42501 on the real `zeroship_auth` role and takes every erasure with it
// - the failure mode this corpus already recorded once, when the reaper read
// control-owned tables it had no privilege on.
//
// This does NOT move the erasure preflight into auth. The reaper still asks the
// control plane over HTTP for the money rule and the per-organization remedy;
// what it reads here is only what a row lock plus an owner count need, and only
// about the human it is erasing.
//
// UPDATE, and why it is a COLUMN grant. PostgreSQL requires the UPDATE
// privilege for `SELECT ... FOR UPDATE` - SELECT alone is refused, and so is
// SELECT with DELETE. A column-level UPDATE satisfies the row lock while
// granting no ability to write any other column, so the auth role can serialize
// against a departure without being able to rename, re-slug or dissolve an
// organization. The grant DSL has no column target, hence the raw island.
export default {
  name: "auth_organization_ownership_fence",
  schema() {
    grant({
      privileges: ["select"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["organizations", "organization_members"],
      },
      to: ["zeroship_auth"],
    });
    raw({
      sql: "GRANT UPDATE (id) ON zeroship.organizations TO zeroship_auth",
      reason:
        "PostgreSQL requires UPDATE for a row lock; the column form grants the lock and no writable column",
    });
  },
};
