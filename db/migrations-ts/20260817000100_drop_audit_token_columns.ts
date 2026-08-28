import { table } from "zero-migrate";

// Both columns recorded the `zeroship.permission_tokens` row a request
// authenticated with. A PAT was the only bearer that ever carried one - an
// OAuth access token is identified by its own claims and sets neither - so with
// PATs removed these are NULL on every future row and can only mislead a reader
// into thinking some other credential class populates them.
//
// Separate from the table drop on purpose: that one removes an authority, this
// one removes its trace, and the two should be reviewable apart.
export default {
  name: "drop_audit_token_columns",
  schema() {
    table("authz_decisions", { schema: "zeroship" }).column("token_id").drop({ ifExists: true });
    table("app_audit", { schema: "zeroship" }).column("actor_token_id").drop({ ifExists: true });
  },
};
