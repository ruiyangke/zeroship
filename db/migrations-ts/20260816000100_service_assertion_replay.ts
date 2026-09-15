import { table, t, grant, revoke, schema } from "@zeroship/migrate";

// The `jti` single-use cache behind the JWT service-assertion mechanism
// (crates/zeroship-core/src/service_assertion.rs, crates/zeroship-authn/src/service_replay.rs).
//
// WHY A TABLE. OIDC Core section 9 makes a service assertion's `jti` REQUIRED
// and single use a MUST; skipping it is CVE-2020-15222. Single use only holds
// if the claim is settled by something EVERY replica of the verifying service
// shares -- otherwise an attacker replaying a captured assertion simply retries
// until a different replica answers, and "single use" is really "single use per
// replica". This deployment already has exactly one shared, strongly consistent
// store, so the cache is a table rather than new infrastructure.
//
// WHY ITS OWN SCHEMA, AND NOT `zeroship`. The `zeroship` schema holds platform
// STATE -- apps, users, deploys, grants, billing -- and authority over it is
// authority over the platform. That is why
// crates/zeroship-migrate-adapter/tests/platform_migrate.rs (DELETED 2026-08-28
// in ccda4bb42, with the platform-migrate binary; nothing asserts this today)
// asserted a BLANKET
// invariant: `zeroship_worker`, the login a process running creator code holds,
// has no write privilege on ANY relation in `zeroship`.
//
// This table records replay claims,
// conferring nothing (see the grant note below on why a `jti` is not a
// credential), and its writer set is different in kind: `zeroship` is written by
// the control plane, while every service that VERIFIES an assertion writes here,
// worker included. Putting it in `zeroship` made one schema carry two trust
// zones under one grant policy, and the worker's write grant collided with the
// invariant head-on. Splitting the zones is the fix; narrowing the invariant to
// "the worker writes nothing except the things it writes" would not be one.
//
// `service_authn` is named for the zone (state of the service-to-service
// authentication MECHANISM) rather than the product, so it cannot be misread as
// the role `zeroship_auth`. No role has it on `search_path`
// (db/migrations-ts/20260702000100_schema_roles_extensions.ts), so every
// statement against it is schema-qualified, which is deliberate.
//
// The schema is on the platform charter's namespace allowlist
// (crates/zeroship-migrate-server/policies/platform.policy.toml) because
// lowering refuses a table outside it. That widening is bounded from the other
// side: platform_migrate.rs asserted this schema holds EXACTLY this table and
// that the worker holds no CREATE on it, so the second zone could not grow into
// the collision the first one hit. That bound is UNHELD since the file was
// deleted; the widening above now rests on review alone.
//
// The replay key and expiry determine whether a claim is still live. The
// claim statement reports its verdict through the affected row count.
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
const SCHEMA = "service_authn";

export default {
  name: "service_assertion_replay",
  schema() {
    schema(SCHEMA).create({ ifNotExists: true });

    table("service_assertion_replay", { schema: SCHEMA }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        replay_key: t.text().notNull(),
        expires_at: t.timestamp().notNull(),
      },
      primaryKey: ["id"],
    });
    table("service_assertion_replay", { schema: SCHEMA }).unique("service_assertion_replay_natural_key").add({ columns: ["replay_key"] });

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

    // Reaching the table needs USAGE on the schema holding it. No role carries
    // `service_authn` on its `search_path`, so this grants reach and nothing else.
    grant({
      privileges: ["usage"],
      on: { kind: "schema", names: [SCHEMA] },
      to: ["zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_auth"],
    });

    // A newly created schema grants CREATE to nobody but its owner, so this is a
    // no-op today. It is written down because the whole point of the split is
    // that a grantee cannot add relations to this zone, and a privilege that is
    // only absent by default is one a later `GRANT ALL` restores silently.
    revoke({
      privileges: ["create"],
      on: { kind: "schema", names: [SCHEMA] },
      from: ["zeroship_control", "zeroship_gateway", "zeroship_worker", "zeroship_auth"],
    });
  },
};
