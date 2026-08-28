import { grant } from "@zeroship/migrate";

// Auth reads `principal_grants` when it decides what a platform token may be
// minted for. The grant was originally added by EDITING 20260702000900_grants.ts
// in place (e0326ab37), which is why it is here instead: that file had already
// been applied to a deployed database, and the runner's checksum guard refuses
// any file whose bytes changed after it was journalled
// (crates/zeroship-migrate-adapter/src/platform.rs, PlatformMigrateError::
// ChecksumMismatch). Re-landing the delta forward is what converges a fresh
// database and a deployed one on the same end state.
//
// This is a WIDENING of the same statement's name list, not a new privilege
// class: 20260702000900 already grants zeroship_auth select on `app_scope_defs`
// in exactly this shape. A second GRANT for the added table is equivalent to
// the edited one-liner, because GRANT is additive and per-(grantee, relation).
export default {
  name: "auth_principal_grants_select",
  schema() {
    grant({
      privileges: ["select"],
      on: { kind: "table", schema: "zeroship", names: ["principal_grants"] },
      to: ["zeroship_auth"],
    });
  },
};
