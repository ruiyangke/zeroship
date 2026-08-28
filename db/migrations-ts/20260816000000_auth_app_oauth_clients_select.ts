import { grant } from "@zeroship/migrate";

// Auth resolves an OAuth client's optional app extension when it selects the
// token subject policy. The first-party CLI registration check also proves the
// reserved client has no app extension. Both reads require this narrow grant.
export default {
  name: "auth_app_oauth_clients_select",
  schema() {
    grant({
      privileges: ["select"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["app_oauth_clients"],
      },
      to: ["zeroship_auth"],
    });
  },
};
