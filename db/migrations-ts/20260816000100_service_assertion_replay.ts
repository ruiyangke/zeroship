import { table, t, grant } from "@zeroship/migrate";

export const name = "service_assertion_replay";

// The `jti` single-use cache behind the JWT service-assertion mechanism
// (crates/core/src/service_assertion.rs, crates/authn/src/service_replay.rs).
//
// WHY A TABLE. OIDC Core section 9 makes a service assertion's `jti` REQUIRED
// and single use a MUST; skipping it is CVE-2020-15222. Single use only holds
// if the claim is settled by something EVERY replica of the verifying service
// shares -- otherwise an attacker replaying a captured assertion simply retries
// until a different replica answers, and "single use" is really "single use per
// replica". This deployment already has exactly one shared, strongly consistent
// store, so the cache is a table rather than new infrastructure.
//
// WHY THESE TWO COLUMNS AND NOTHING ELSE. The store answers one question --
// "has this key been claimed, and is that claim still live" -- so it carries
// the key and the instant the claim may be dropped. Nothing here is read back
// INTO application code: the claim statement reports its verdict as an
// affected row count and returns no rows. That is a statement about RETURNING,
// and it is NOT a reason to withhold SELECT -- see the grant below.
//
// `replay_key` is `<iss>|<jti>`, scoped by issuer so one service cannot burn
// another service's `jti`, and so a single table is safe to share across
// callees. Both halves are character-restricted by the verifier before they
// reach here (the issuer excludes `|`, the `jti` is [A-Za-z0-9_-] and at most
// 64 characters), so the joined key is unambiguous and bounded.
//
// `expires_at` is the end of the window during which the assertion it covers
// can still be accepted -- its `exp` plus the verifier's clock-skew tolerance.
// A row deleted before that instant would make the assertion replayable while
// it is still valid, which is exactly the window the mechanism exists to close.
// Rows past it are reclaimed in place by the next claim of the same key, so the
// sweep is housekeeping and not a correctness dependency.
const SCHEMA = "zeroship";

export function up() {
  table("service_assertion_replay", { schema: SCHEMA }).create({
    columns: {
      replay_key: t.text().notNull(),
      expires_at: t.timestamp().notNull(),
    },
    primaryKey: ["replay_key"],
  });

  // The sweep is `DELETE ... WHERE expires_at <= now()`. Without this index it
  // is a sequential scan over every live claim.
  table("service_assertion_replay", { schema: SCHEMA })
    .index("service_assertion_replay_expiry_idx")
    .add({ on: ["expires_at"] });

  // INSERT and UPDATE are both needed by the single claim statement: it is
  // `INSERT ... ON CONFLICT DO UPDATE ... WHERE`, and PostgreSQL requires
  // UPDATE privilege to PLAN that arm even when it never fires. This is the
  // same class as 20260812000000 and 20260812000200, where insert-only grants
  // made production upserts fail at plan time. DELETE is for the sweep.
  //
  // SELECT IS ALSO REQUIRED, and an earlier revision of this file argued it
  // away. PostgreSQL's rule is about COLUMN READS, not about RETURNING:
  // UPDATE and DELETE need SELECT on every column read in an expression or a
  // condition. The claim reads `expires_at` in the DO UPDATE arm's WHERE and
  // the sweep reads it in its own WHERE, so both statements need it. MEASURED
  // on a scratch database built by zeroship-platform-migrate from this
  // directory: with insert/update/delete only, every one of the four roles
  // below got `permission denied for table service_assertion_replay` for BOTH
  // statements; adding select made all eight succeed. Controls, same role,
  // one variable each: dropping the WHERE from the upsert is still denied
  // (the DO UPDATE arm alone needs it), and a DELETE with no WHERE succeeds.
  // Withholding it would have failed 100% of inbound service-to-service calls
  // the day a service was wired to PostgresReplayStore, via the verifier's
  // fail-closed arm, with a Postgres permission error in the log and nothing
  // naming grants.
  //
  // What SELECT concedes is bounded: a service that already holds insert and
  // delete here can read the `<iss>|<jti>` keys of other services' in-flight
  // assertions. A `jti` is not a credential -- the assertion carrying it is
  // signed, single use, and bound to its own audience -- and every grantee is
  // itself one of the services whose keys are in the table.
  //
  // Granted to every role that runs a service which VERIFIES assertions. There
  // is no `zeroship_migrated` role in db/migrations-ts/20260702000100_schema_
  // roles_extensions.ts, so migrated is absent here; it will need a grant when
  // it gets a role, and that is called out rather than pre-granted to a role
  // that does not exist.
  grant({
    privileges: ["select", "insert", "update", "delete"],
    on: {
      kind: "table",
      schema: SCHEMA,
      names: ["service_assertion_replay"],
    },
    to: ["zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_auth"],
  });
}

export function down() {

}
