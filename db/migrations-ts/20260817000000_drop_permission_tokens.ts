import { table } from "zero-migrate";

// Personal access tokens are gone. The table was the storage half of a SECOND
// issuance authority: control minted a 365-day credential with its own signing
// key and validated it against these rows, while the OP is meant to be the only
// issuer on the platform. The routes, the verifier and the signing key are
// removed in the same change; this drops what they wrote to.
//
// `kind` allowed 'pat' and 'oauth_grant', but every writer and reader in the
// tree pinned `kind = 'pat'`; the only occurrence of the other value is the
// CHECK constraint that admits it. No live surface loses storage here.
export default {
  name: "drop_permission_tokens",
  schema() {
    table("permission_tokens", { schema: "zeroship" }).drop({ ifExists: true, cascade: true });
  },
};
