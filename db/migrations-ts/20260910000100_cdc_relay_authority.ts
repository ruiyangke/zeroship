import { grant, raw, role } from "@zeroship/migrate";

// Logical decoding belongs to the relay process. Workers execute creator code
// and authenticate relay subscriptions with their enrolled instance identity.
export default {
  name: "cdc_relay_authority",
  schema() {
    role("zeroship_cdc").create({ login: true, password: "zeroship_cdc", ifNotExists: true });
    raw({
      sql: "ALTER ROLE zeroship_cdc WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT REPLICATION NOBYPASSRLS",
      reason: "the relay owns logical decoding without ordinary tenant table privileges",
    });
    grant({
      privileges: ["usage"],
      on: { kind: "schema", names: ["zeroship"] },
      to: ["zeroship_cdc"],
    });
    raw({
      sql: "GRANT SELECT (id, status, public_key) ON zeroship.worker_instances TO zeroship_cdc",
      reason: "the relay verifies enrolled worker identities and observes revocation",
    });
    raw({
      sql: "ALTER ROLE zeroship_worker WITH NOREPLICATION NOBYPASSRLS",
      reason: "workers receive committed invalidations from the relay",
    });
  },
};
