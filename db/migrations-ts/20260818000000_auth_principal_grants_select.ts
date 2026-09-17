import { grant } from "@zeroship/migrate";

// Auth reads `principal_grants` when it decides what a platform token may be
// minted for. The grant is re-landed forward here rather than by editing
// 20260702000900_grants.ts in place: that file had already been journalled, so
// re-applying it after an edit would be refused, and re-landing the delta is
// what converges a fresh database and a deployed one on the same end state.
//
// WHAT THE GUARD COMPARES. The guard is over `Checksum::of_ir`, the digest of
// the canonical OP LIST plus flags, owner_app, depends_on, supersedes and
// preconditions (crates/zeroship-migrate-ir/src/migration.rs); the journal key
// excludes content outright (crates/zeroship-migrate-core/src/render/lower.rs).
// It is not a byte digest: a `//` comment edit leaves the op list byte-identical
// and is invisible to the guard, while editing one `reason:` string inside a
// `raw({...})` op value moves it. So comments here may be repaired freely; an op
// value, or the exported `name:`, may not.
//
// This WIDENS the same statement's name list, not a new privilege class:
// 20260702000900 already grants zeroship_auth select on `app_scope_defs` in
// exactly this shape, and GRANT is additive and per-(grantee, relation).
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
