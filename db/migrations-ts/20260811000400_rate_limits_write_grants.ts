import { grant } from "zero-migrate";

// The shared token-bucket limiter could not write its own bucket, so the
// throttle failed closed and took the whole control admin API with it.
//
// MEASURED 2026-08-11 on a live deploy: every `/api/*` route on the control
// plane answered `503 {"error":"rate limit unavailable"}`. Each of those routes
// calls `admin_rate_limit` FIRST (crates/control/src/env_handlers.rs), and when
// the limiter itself errors, `http_util::rate_limit` returns 503 rather than
// letting the request through - correct behaviour, fatal input. The log line
// was `control: shared rate-limit consume failed ... ratelimit consume
// control:admin:ip:...: db error`.
//
// The store runs ONE statement (crates/auth/src/store/ratelimit.rs):
//     INSERT INTO zeroship.rate_limits ... ON CONFLICT (bucket_key) DO UPDATE ...
// which PostgreSQL requires BOTH insert AND update privileges for. It checks
// the update privilege for the DO UPDATE clause at plan time, so the denial
// happens on the FIRST call, not only once a bucket already exists - which is
// why this never presented as an intermittent fault.
//
// 20260702000900_grants.ts granted:
//     zeroship_control  select, delete           -> no insert, no update
//     zeroship_auth     select, insert, delete   -> no update
// Verified by SET ROLE against the live database, one variable changed per run:
// the same statement succeeds as superuser and is denied for BOTH roles; a
// plain INSERT with no on-conflict clause is ALSO denied for control, which is
// what separates "missing update" from "missing insert" for it.
//
// `delete` is left in place for both: the limiter has a sweep path, and this
// migration is scoped to the privileges the consume statement needs.
export default {
  name: "rate_limits_write_grants",
  schema() {
    grant({
      privileges: ["insert", "update"],
      on: { kind: "table", schema: "zeroship", names: ["rate_limits"] },
      to: ["zeroship_control"],
    });
    grant({
      privileges: ["update"],
      on: { kind: "table", schema: "zeroship", names: ["rate_limits"] },
      to: ["zeroship_auth"],
    });
  },
};
