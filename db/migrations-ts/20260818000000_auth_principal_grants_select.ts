import { grant } from "@zeroship/migrate";

// Auth reads `principal_grants` when it decides what a platform token may be
// minted for. The grant was originally added by EDITING 20260702000900_grants.ts
// in place (e0326ab37), which is why it is here instead: that file had already
// been applied to a deployed database, and re-applying it would have been
// refused as checksum drift. Re-landing the delta forward is what converges a
// fresh database and a deployed one on the same end state.
//
// WHAT THE GUARD ACTUALLY COMPARES, corrected 2026-09-04. This comment used to
// say it "refuses any file whose bytes changed after it was journalled", citing
// crates/zeroship-migrate-adapter/src/platform.rs (DELETED 2026-08-28 in
// ccda4bb42) and its PlatformMigrateError::ChecksumMismatch. BOTH HALVES WERE
// WRONG. The crate is gone, and the guard that ships is not over bytes: it is
// over `Checksum::of_ir`, the digest of the canonical OP LIST plus flags,
// owner_app, depends_on, supersedes and preconditions
// (crates/zeroship-migrate-ir/src/migration.rs). The journal key excludes
// content outright (crates/zeroship-migrate-core/src/render/lower.rs).
//
// The difference is not academic and was measured by driving the recorder over
// a sibling file: a `//` COMMENT edit leaves the op list byte-identical and is
// invisible to the guard, while editing one `reason:` string inside a
// `raw({...})` op value moves it. So comments here may be repaired freely; an
// op value, or the exported `name:`, may not.
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
