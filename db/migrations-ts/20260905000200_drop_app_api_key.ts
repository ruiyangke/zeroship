import { table } from "@zeroship/migrate";

// The app-level API key is deleted, and it is not coming back. This header is
// the record of WHY, because "there is no app key" invites someone to build one.
//
// WHY THE PLATFORM DOES NOT OWN AN APP-LEVEL KEY
//
// An app-level key is a master key to a whole application. It cannot be scoped
// to a user or to an operation, it cannot be rotated for one consumer without
// breaking every other, and a leak of it leaks every consumer's access at once.
// Those are properties of the SHAPE, not of any implementation of it - a better
// hash, a shorter lifetime or a prefix convention changes none of them. So the
// platform does not mint one.
//
// A creator who wants API tokens for their app builds them in app code, where
// the authorization model that knows what a token may DO already lives. The
// platform cannot know that, which is the deeper reason this belongs there.
//
// IF LONG-LIVED PROGRAMMATIC ACCESS BECOMES A PLATFORM FEATURE, the right shape
// is a session row with a machine kind. A session inherits revocation, listing,
// expiry and audit from the model that already carries them; a key column hangs
// off nothing and has to reinvent all four. Build that, not this.
//
// WHAT THE PLAINTEXT COLUMN ACTUALLY WAS
//
// `apps.api_key_hash` and the gateway's `check_api_key` went in
// 20260905000100_drop_app_api_key_hash.ts. That file kept the PLAINTEXT half on
// the grounds that `AppRecord.api_key` "still has production readers". Measured
// before writing this file, that claim was weaker than it read, and it is
// recorded here rather than quietly dropped:
//
//   - NO BRANCH EXISTED. Four sites touched the value. `Registry::create_app`
//     minted a UUID for the INSERT, `row_to_record` copied it into the struct,
//     `dev_provision` printed it, and one test fixture built a record with a
//     placeholder. No comparison, no equality, no policy lookup, no refusal.
//   - NOTHING COULD PRESENT A CREDENTIAL TO IT. No `X-Api-Key` reader existed in
//     any Rust or TypeScript source. The header a dozen shell harnesses sent was
//     read by nothing, and two of them sent an empty string and still passed.
//   - THE VALUE NEVER LEFT THE PROCESS. `#[serde(skip_serializing)]` plus a
//     create-response test asserting its absence under any field name.
//   - NO OTHER CRATE COULD SEE IT. The gateway and worker consume `RouteEntry`
//     and its siblings; none of them carried the field.
//
// So "production readers" meant a SELECT list feeding a struct field nothing
// consumed, plus a println in a dev-provisioning tool, plus harnesses that
// scraped that printed line back into a header no server read. That is a dev
// print, not a decision. The whole chain goes in the change that lands this.
export default {
  name: "drop_app_api_key",
  schema() {
    table("apps", { schema: "zeroship" }).column("api_key").drop({ ifExists: true });
  },
};
